//! Start-up on Android (Meta Quest): app folders, then the same OpenXR loop
//! as on the Frame, with the activity's events handled between frames.

use android_activity::{AndroidApp, InputStatus, MainEvent, PollEvent};
use std::sync::OnceLock;
use std::time::Duration;

static APP: OnceLock<AndroidApp> = OnceLock::new();

/// Shared storage, where videos copied to the headset land (Movies,
/// Download, ...): the home folder for the default library folders.
const SHARED_STORAGE: &str = "/storage/emulated/0";

/// Runs FramePlayer in the activity `app`; called once from
/// `android_main`, ends the process when done.
pub fn run(app: AndroidApp) {
    // Settings, library and logs live in the app's private storage; the
    // XDG variables point fp_core::dirs there.
    if let Some(private) = app.internal_data_path() {
        // SAFETY: no other threads exist yet.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", private.join("config"));
            std::env::set_var("XDG_DATA_HOME", private.join("data"));
            std::env::set_var("XDG_CACHE_HOME", private.join("cache"));
            std::env::set_var("HOME", SHARED_STORAGE);
        }
    }
    // The activity itself (ndk_context only has the application, which
    // can't ask for permissions and isn't what OpenXR wants).
    let (vm, activity) = (app.vm_as_ptr(), app.activity_as_ptr());
    let _ = APP.set(app);
    crate::logger::init(false);
    // SAFETY: both stay valid while APP holds the AndroidApp, i.e. for good.
    unsafe { fp_xr::set_android_activity(vm, activity) };
    // SAFETY: as above.
    if let Err(e) = unsafe { ask_for_storage(vm, activity) } {
        log::warn!("storage permission: {e}");
    }
    fp_media::init_logging();
    log::info!(
        "FramePlayer {} starting (Android)",
        env!("CARGO_PKG_VERSION")
    );
    if let Err(e) = crate::run_xr(&crate::Args::default()) {
        log::error!("{e}");
    }
    // A finished activity's process may be kept and android_main called
    // again, with every static still set: start clean instead.
    std::process::exit(0);
}

/// Asks for access to shared storage (videos in Movies, Download, ...)
/// unless already given; Android shows its own dialog. Without it the
/// library only has network sources.
///
/// # Safety
/// `vm` and `activity` must be the app's Java VM and activity.
unsafe fn ask_for_storage(
    vm: *mut std::ffi::c_void,
    activity: *mut std::ffi::c_void,
) -> Result<(), String> {
    use jni_sys::{JNI_OK, JNIEnv, jobject, jvalue};
    use std::ffi::c_void;
    const PERMISSION: &std::ffi::CStr = c"android.permission.READ_EXTERNAL_STORAGE";
    let vm = vm as *mut jni_sys::JavaVM;
    let activity = activity as jobject;
    // SAFETY (all calls): JNI functions from the VM's own table, on a thread
    // attached here, with references created in this frame.
    unsafe {
        let mut env: *mut JNIEnv = std::ptr::null_mut();
        let attach = (**vm).AttachCurrentThread.ok_or("no AttachCurrentThread")?;
        if attach(
            vm,
            &mut env as *mut _ as *mut *mut c_void,
            std::ptr::null_mut(),
        ) != JNI_OK
        {
            return Err("cannot attach to the Java VM".into());
        }
        let f = &**env;
        let (Some(get_class), Some(method), Some(string), Some(call_int), Some(call_void)) = (
            f.GetObjectClass,
            f.GetMethodID,
            f.NewStringUTF,
            f.CallIntMethodA,
            f.CallVoidMethodA,
        ) else {
            return Err("JNI functions missing".into());
        };
        let (Some(find_class), Some(new_array), Some(check_exc), Some(clear_exc)) = (
            f.FindClass,
            f.NewObjectArray,
            f.ExceptionCheck,
            f.ExceptionClear,
        ) else {
            return Err("JNI functions missing".into());
        };
        let class = get_class(env, activity);
        let perm = string(env, PERMISSION.as_ptr());
        let check = method(
            env,
            class,
            c"checkSelfPermission".as_ptr(),
            c"(Ljava/lang/String;)I".as_ptr(),
        );
        // A missing method leaves an exception pending; calling it anyway
        // makes the VM abort the whole app.
        if check.is_null() {
            clear_exc(env);
            return Err("no checkSelfPermission".into());
        }
        let granted = call_int(env, activity, check, [jvalue { l: perm }].as_ptr());
        if check_exc(env) != 0 {
            clear_exc(env);
            return Err("checkSelfPermission failed".into());
        }
        if granted == 0 {
            log::info!("storage access already given");
            return Ok(());
        }
        let strings = new_array(env, 1, find_class(env, c"java/lang/String".as_ptr()), perm);
        let request = method(
            env,
            class,
            c"requestPermissions".as_ptr(),
            c"([Ljava/lang/String;I)V".as_ptr(),
        );
        if request.is_null() {
            clear_exc(env);
            return Err("no requestPermissions".into());
        }
        call_void(
            env,
            activity,
            request,
            [jvalue { l: strings }, jvalue { i: 1 }].as_ptr(),
        );
        if check_exc(env) != 0 {
            clear_exc(env);
            return Err("requestPermissions failed".into());
        }
        log::info!("asked for storage access");
        Ok(())
    }
}

/// Handles the activity's pending events without waiting. Returns true
/// once the activity is being destroyed.
pub(crate) fn pump() -> bool {
    let Some(app) = APP.get() else {
        return false;
    };
    let mut destroyed = false;
    app.poll_events(Some(Duration::ZERO), |event| match event {
        PollEvent::Main(MainEvent::Destroy) => destroyed = true,
        // Controllers come through OpenXR; key and touch events are only
        // drained so the system never waits on them.
        PollEvent::Main(MainEvent::InputAvailable) => {
            if let Ok(mut events) = app.input_events_iter() {
                while events.next(|_| InputStatus::Unhandled) {}
            }
        }
        _ => {}
    });
    destroyed
}
