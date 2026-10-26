//! FramePlayer for Meta Quest: `libframeplayer_quest.so`, the library the
//! APK's NativeActivity loads. The app itself is the `frameplayer` crate;
//! this only gives Android its entry point. See `packaging/quest`.
#![cfg(target_os = "android")]

#[unsafe(no_mangle)]
fn android_main(app: android_activity::AndroidApp) {
    frameplayer::android::run(app);
}
