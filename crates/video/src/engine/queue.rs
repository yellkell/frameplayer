//! Decode-ahead frame queue between the decode thread and the render thread.
//!
//! The decode thread pushes frames in presentation order (non-blocking,
//! bounded to the decode-ahead depth). The render thread calls
//! [`FrameQueue::frame_for`] with the media time predicted for the next
//! display refresh; the newest frame whose presentation time has arrived
//! becomes current, older ones are dropped (counted as late), and frames
//! still in the future stay queued.

use crate::decode::DecodedFrame;
use fp_core::MediaTime;
use parking_lot::{Condvar, Mutex};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

/// A frame ready for presentation.
#[derive(Debug)]
pub struct VideoFrame {
    pub frame: DecodedFrame,
    pub pts: MediaTime,
    pub duration: MediaTime,
    /// Seek generation the frame belongs to.
    pub serial: u64,
}

#[derive(Default)]
struct Inner {
    frames: VecDeque<Arc<VideoFrame>>,
    current: Option<Arc<VideoFrame>>,
    serial: u64,
    late_dropped: u64,
    presented: u64,
}

pub struct FrameQueue {
    inner: Mutex<Inner>,
    space: Condvar,
    capacity: usize,
}

/// Counters for the performance HUD.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct QueueStats {
    pub queued: usize,
    pub late_dropped: u64,
    pub presented: u64,
}

impl FrameQueue {
    pub fn new(capacity: usize) -> Self {
        FrameQueue {
            inner: Mutex::new(Inner::default()),
            space: Condvar::new(),
            capacity: capacity.max(1),
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn len(&self) -> usize {
        self.inner.lock().frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn is_full(&self) -> bool {
        self.len() >= self.capacity
    }

    pub fn serial(&self) -> u64 {
        self.inner.lock().serial
    }

    /// Queue a frame; gives it back when the queue is full. Frames from an
    /// older seek generation are silently discarded.
    pub fn try_push(&self, f: VideoFrame) -> Result<(), VideoFrame> {
        let mut s = self.inner.lock();
        if f.serial != s.serial {
            return Ok(());
        }
        if s.frames.len() >= self.capacity {
            return Err(f);
        }
        // Keep presentation order even if a decoder emits slightly out of order.
        let pos = s.frames.partition_point(|q| q.pts <= f.pts);
        s.frames.insert(pos, Arc::new(f));
        Ok(())
    }

    /// Block until there is room or `timeout` elapses.
    pub fn wait_for_space(&self, timeout: Duration) -> bool {
        let mut s = self.inner.lock();
        if s.frames.len() < self.capacity {
            return true;
        }
        self.space.wait_for(&mut s, timeout);
        s.frames.len() < self.capacity
    }

    /// The frame to show at media time `t`. Returns the current frame
    /// (possibly unchanged) or `None` if nothing has been presentable yet.
    pub fn frame_for(&self, t: MediaTime) -> Option<Arc<VideoFrame>> {
        let mut s = self.inner.lock();
        let mut advanced = false;
        while s.frames.front().is_some_and(|f| f.pts <= t) {
            let f = s.frames.pop_front().unwrap();
            if advanced {
                s.late_dropped += 1;
            }
            s.current = Some(f);
            advanced = true;
        }
        if advanced {
            s.presented += 1;
            self.space.notify_all();
        }
        s.current.clone()
    }

    /// Drop queued frames that ended before `t` (they can never be shown);
    /// keeps decoding moving when the render thread falls behind.
    pub fn drop_late(&self, t: MediaTime) -> usize {
        let mut s = self.inner.lock();
        let mut n = 0;
        while s.frames.front().is_some_and(|f| f.pts + f.duration <= t) {
            s.frames.pop_front();
            n += 1;
        }
        if n > 0 {
            s.late_dropped += n as u64;
            self.space.notify_all();
        }
        n
    }

    /// Make the next queued frame current regardless of time (frame step).
    pub fn step(&self) -> Option<Arc<VideoFrame>> {
        let mut s = self.inner.lock();
        let f = s.frames.pop_front()?;
        s.current = Some(f.clone());
        s.presented += 1;
        self.space.notify_all();
        Some(f)
    }

    pub fn current(&self) -> Option<Arc<VideoFrame>> {
        self.inner.lock().current.clone()
    }

    pub fn next_pts(&self) -> Option<MediaTime> {
        self.inner.lock().frames.front().map(|f| f.pts)
    }

    pub fn last_pts(&self) -> Option<MediaTime> {
        self.inner.lock().frames.back().map(|f| f.pts)
    }

    /// Drop queued frames and start a new seek generation. The current
    /// frame stays on screen until a new one is presentable.
    pub fn flush(&self) -> u64 {
        let mut s = self.inner.lock();
        s.frames.clear();
        s.serial += 1;
        self.space.notify_all();
        s.serial
    }

    /// Drop everything including the current frame (close).
    pub fn clear(&self) {
        let mut s = self.inner.lock();
        s.frames.clear();
        s.current = None;
        s.serial += 1;
        self.space.notify_all();
    }

    pub fn stats(&self) -> QueueStats {
        let s = self.inner.lock();
        QueueStats {
            queued: s.frames.len(),
            late_dropped: s.late_dropped,
            presented: s.presented,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::{CpuFrame, PixelFormat};

    fn vf(ms: i64, serial: u64) -> VideoFrame {
        let pts = MediaTime::from_millis(ms);
        VideoFrame {
            frame: DecodedFrame::Cpu(CpuFrame {
                format: PixelFormat::Nv12,
                width: 2,
                height: 2,
                planes: vec![vec![0; 4], vec![0; 2]],
                strides: vec![2, 2],
                pts,
            }),
            pts,
            duration: MediaTime::from_millis(40),
            serial,
        }
    }

    #[test]
    fn selection_by_display_time() {
        let q = FrameQueue::new(4);
        for ms in [0, 40, 80, 120] {
            q.try_push(vf(ms, 0)).unwrap();
        }
        assert!(q.try_push(vf(160, 0)).is_err(), "bounded");
        assert!(q.frame_for(MediaTime::from_millis(-5)).is_none());
        assert_eq!(
            q.frame_for(MediaTime::from_millis(10)).unwrap().pts,
            MediaTime::ZERO
        );
        // Same refresh window: unchanged.
        assert_eq!(
            q.frame_for(MediaTime::from_millis(30)).unwrap().pts,
            MediaTime::ZERO
        );
        // Late display: 40 is skipped, 80 shown.
        assert_eq!(
            q.frame_for(MediaTime::from_millis(95)).unwrap().pts,
            MediaTime::from_millis(80)
        );
        let st = q.stats();
        assert_eq!((st.queued, st.late_dropped, st.presented), (1, 1, 2));
        assert!(q.wait_for_space(Duration::from_millis(1)));
        q.try_push(vf(160, 0)).unwrap();
        // Queue is [120, 160]: 120 ends at 160 ≤ 165 → late; 160 covers 165 → kept.
        assert_eq!(q.drop_late(MediaTime::from_millis(165)), 1);
        assert_eq!(q.next_pts(), Some(MediaTime::from_millis(160)));
    }

    #[test]
    fn flush_discards_stale_generation() {
        let q = FrameQueue::new(4);
        q.try_push(vf(0, 0)).unwrap();
        q.frame_for(MediaTime::ZERO);
        let serial = q.flush();
        assert_eq!(serial, 1);
        q.try_push(vf(500, 0)).unwrap(); // stale: dropped
        assert!(q.is_empty());
        q.try_push(vf(1000, 1)).unwrap();
        // Old frame stays current until the new one is due.
        assert_eq!(
            q.frame_for(MediaTime::from_millis(900)).unwrap().pts,
            MediaTime::ZERO
        );
        assert_eq!(
            q.frame_for(MediaTime::from_millis(1000)).unwrap().pts,
            MediaTime::from_millis(1000)
        );
    }

    #[test]
    fn step_and_ordering() {
        let q = FrameQueue::new(4);
        q.try_push(vf(80, 0)).unwrap();
        q.try_push(vf(40, 0)).unwrap();
        assert_eq!(q.next_pts(), Some(MediaTime::from_millis(40)));
        assert_eq!(q.step().unwrap().pts, MediaTime::from_millis(40));
        assert_eq!(q.current().unwrap().pts, MediaTime::from_millis(40));
        q.clear();
        assert!(q.current().is_none());
    }
}
