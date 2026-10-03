//! Channel layouts understood by the pipeline.

/// Ambisonic channel normalisation / ordering convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AmbisonicNorm {
    /// AmbiX: ACN channel order, SN3D normalisation (YouTube / spatial-media).
    AmbiX,
    /// Furse-Malham: WXYZ RSTUV KLMNOPQ order and weights (orders ≤ 3).
    FuMa,
}

/// How the channels of an interleaved PCM stream are to be interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChannelLayout {
    Mono,
    Stereo,
    /// L R C LFE Ls Rs (WAV / SMPTE order).
    Surround51,
    /// L R C LFE Lb Rb Ls Rs (WAV / SMPTE order).
    Surround71,
    /// Full-sphere ambisonics of the given order: `(order + 1)^2` channels.
    Ambisonic {
        order: u8,
        norm: AmbisonicNorm,
    },
    /// Anything else: the first two channels are used as L/R.
    Other(u16),
}

impl ChannelLayout {
    /// Guess a layout from a channel count and optional ambisonic order
    /// signalled by the container (e.g. an MP4 `SA3D` box).
    pub fn guess(channels: u16, ambisonic_order: Option<u8>) -> ChannelLayout {
        if let Some(order) = ambisonic_order {
            let n = (order as u16 + 1).pow(2);
            if channels >= n && order >= 1 {
                return ChannelLayout::Ambisonic {
                    order,
                    norm: AmbisonicNorm::AmbiX,
                };
            }
        }
        match channels {
            1 => ChannelLayout::Mono,
            2 => ChannelLayout::Stereo,
            6 => ChannelLayout::Surround51,
            8 => ChannelLayout::Surround71,
            n => ChannelLayout::Other(n),
        }
    }

    pub fn channels(self) -> usize {
        match self {
            ChannelLayout::Mono => 1,
            ChannelLayout::Stereo => 2,
            ChannelLayout::Surround51 => 6,
            ChannelLayout::Surround71 => 8,
            ChannelLayout::Ambisonic { order, .. } => (order as usize + 1).pow(2),
            ChannelLayout::Other(n) => n as usize,
        }
    }

    pub fn is_ambisonic(self) -> bool {
        matches!(self, ChannelLayout::Ambisonic { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guess_layouts() {
        assert_eq!(ChannelLayout::guess(6, None), ChannelLayout::Surround51);
        assert_eq!(
            ChannelLayout::guess(4, Some(1)),
            ChannelLayout::Ambisonic {
                order: 1,
                norm: AmbisonicNorm::AmbiX
            }
        );
        // Signalled order needs enough channels.
        assert_eq!(ChannelLayout::guess(4, Some(2)), ChannelLayout::Other(4));
        assert_eq!(
            ChannelLayout::Ambisonic {
                order: 3,
                norm: AmbisonicNorm::AmbiX
            }
            .channels(),
            16
        );
    }
}
