//! The Meta Quest Store side of the Quest app, through Meta's Platform SDK
//! (`libovrplatformloader.so`, which tools/package-quest.sh takes from the
//! SDK's AAR): the entitlement check the store asks for at start-up, and
//! the passthrough unlock as a durable in-app purchase.
//!
//! A build made for the store (`--features store`) closes when the check
//! fails, as the store requires; other builds only log it, so sideloaded
//! test builds keep working.

use jni_sys::{JNI_OK, JNIEnv, JavaVM, jobject};
use std::ffi::{CStr, c_char, c_int, c_void};
use std::sync::{Mutex, OnceLock};

/// FramePlayer's app on the Meta Horizon dashboard.
const APP_ID: &CStr = c"1361403123734021";
/// The passthrough videos add-on.
const SKU: &CStr = c"passthrough_unlock";

// Message types (OVR_MessageType.h).
const MSG_ENTITLEMENT: u32 = 0x186B58B1;
const MSG_PURCHASES: u32 = 0x3A0F8419;
const MSG_CHECKOUT: u32 = 0x3F9B0D0D;

type Handle = *mut c_void;

/// The Platform SDK functions used here, from the loader.
struct Api {
    initialize: unsafe extern "C" fn(*const c_char, jobject, *mut JNIEnv) -> c_int,
    entitlement: unsafe extern "C" fn() -> u64,
    purchases: unsafe extern "C" fn() -> u64,
    checkout: unsafe extern "C" fn(*const c_char) -> u64,
    pop: unsafe extern "C" fn() -> Handle,
    kind: unsafe extern "C" fn(Handle) -> u32,
    is_error: unsafe extern "C" fn(Handle) -> bool,
    error: unsafe extern "C" fn(Handle) -> Handle,
    error_text: unsafe extern "C" fn(Handle) -> *const c_char,
    purchase_array: unsafe extern "C" fn(Handle) -> Handle,
    array_size: unsafe extern "C" fn(Handle) -> usize,
    array_element: unsafe extern "C" fn(Handle, usize) -> Handle,
    purchase: unsafe extern "C" fn(Handle) -> Handle,
    sku: unsafe extern "C" fn(Handle) -> *const c_char,
    free: unsafe extern "C" fn(Handle),
}

unsafe extern "C" {
    fn dlopen(filename: *const c_char, flags: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn dlerror() -> *const c_char;
}
const RTLD_NOW: c_int = 2;

impl Api {
    // Each symbol's type is the field it fills.
    #[allow(clippy::missing_transmute_annotations)]
    fn load() -> Result<Api, String> {
        // SAFETY: dlopen/dlsym with NUL-terminated names; each symbol is
        // cast to the signature OVR_Platform.h gives it.
        unsafe {
            let lib = dlopen(c"libovrplatformloader.so".as_ptr(), RTLD_NOW);
            if lib.is_null() {
                let e = dlerror();
                return Err(if e.is_null() {
                    "no Platform SDK".into()
                } else {
                    CStr::from_ptr(e).to_string_lossy().into_owned()
                });
            }
            macro_rules! sym {
                ($name:literal) => {{
                    let p = dlsym(lib, concat!($name, "\0").as_ptr().cast());
                    if p.is_null() {
                        return Err(concat!("Platform SDK has no ", $name).into());
                    }
                    std::mem::transmute::<*mut c_void, _>(p)
                }};
            }
            Ok(Api {
                initialize: sym!("ovr_PlatformInitializeAndroid"),
                entitlement: sym!("ovr_Entitlement_GetIsViewerEntitled"),
                purchases: sym!("ovr_IAP_GetViewerPurchases"),
                checkout: sym!("ovr_IAP_LaunchCheckoutFlow"),
                pop: sym!("ovr_PopMessage"),
                kind: sym!("ovr_Message_GetType"),
                is_error: sym!("ovr_Message_IsError"),
                error: sym!("ovr_Message_GetError"),
                error_text: sym!("ovr_Error_GetMessage"),
                purchase_array: sym!("ovr_Message_GetPurchaseArray"),
                array_size: sym!("ovr_PurchaseArray_GetSize"),
                array_element: sym!("ovr_PurchaseArray_GetElement"),
                purchase: sym!("ovr_Message_GetPurchase"),
                sku: sym!("ovr_Purchase_GetSKU"),
                free: sym!("ovr_FreeMessage"),
            })
        }
    }
}

static API: OnceLock<Api> = OnceLock::new();

/// What the store has said so far.
#[derive(Default)]
struct State {
    /// The add-on is owned (bought now or before).
    owned: bool,
    /// A checkout in progress, then how it ended.
    checkout: Option<crate::unlock::Status>,
    /// The entitlement check failed: the app must close.
    refused: bool,
}

static STATE: Mutex<State> = Mutex::new(State {
    owned: false,
    checkout: None,
    refused: false,
});

fn enforce() -> bool {
    cfg!(feature = "store")
}

/// Starts the Platform SDK and asks whether this user owns FramePlayer and
/// which add-ons they have bought.
///
/// # Safety
/// `vm` and `activity` must be the app's Java VM and activity, and this
/// must run on the thread that then calls [`pump`].
pub unsafe fn start(vm: *mut c_void, activity: *mut c_void) {
    let api = match Api::load() {
        Ok(a) => a,
        Err(e) => {
            log::warn!("store: {e}");
            STATE.lock().unwrap().refused = enforce();
            return;
        }
    };
    let vm = vm as *mut JavaVM;
    let mut env: *mut JNIEnv = std::ptr::null_mut();
    // SAFETY: the VM's own table; attaching an attached thread is a no-op.
    let attached = unsafe {
        (**vm).AttachCurrentThread.is_some_and(|attach| {
            attach(
                vm,
                &mut env as *mut _ as *mut *mut c_void,
                std::ptr::null_mut(),
            ) == JNI_OK
        })
    };
    if !attached {
        log::warn!("store: cannot attach to the Java VM");
        STATE.lock().unwrap().refused = enforce();
        return;
    }
    // SAFETY: the activity and this thread's JNIEnv, as the SDK asks.
    let result = unsafe { (api.initialize)(APP_ID.as_ptr(), activity as jobject, env) };
    if result != 0 {
        log::warn!("store: Platform SDK didn't start ({result})");
        STATE.lock().unwrap().refused = enforce();
        return;
    }
    // SAFETY: requests on a started SDK; answers come through pump.
    unsafe {
        (api.entitlement)();
        (api.purchases)();
    }
    let _ = API.set(api);
    log::info!("store: checking entitlement");
}

/// Handles the store's answers. Returns true when the app must close: a
/// store build whose user doesn't own it.
pub fn pump() -> bool {
    if let Some(api) = API.get() {
        loop {
            // SAFETY: messages from the SDK, freed once read.
            unsafe {
                let msg = (api.pop)();
                if msg.is_null() {
                    break;
                }
                handle(api, msg);
                (api.free)(msg);
            }
        }
    }
    STATE.lock().unwrap().refused
}

/// # Safety
/// `msg` must be a message just popped from the SDK.
unsafe fn handle(api: &Api, msg: Handle) {
    // SAFETY (all calls): accessors on a live message and its parts.
    unsafe {
        let error = (api.is_error)(msg).then(|| {
            let e = (api.error_text)((api.error)(msg));
            if e.is_null() {
                String::new()
            } else {
                CStr::from_ptr(e).to_string_lossy().into_owned()
            }
        });
        let mut state = STATE.lock().unwrap();
        match (api.kind)(msg) {
            MSG_ENTITLEMENT => match error {
                None => log::info!("store: entitled"),
                Some(e) => {
                    log::warn!("store: not entitled: {e}");
                    state.refused = enforce();
                }
            },
            MSG_PURCHASES => match error {
                None => {
                    let list = (api.purchase_array)(msg);
                    for i in 0..(api.array_size)(list) {
                        let sku = (api.sku)((api.array_element)(list, i));
                        if !sku.is_null() && CStr::from_ptr(sku) == SKU {
                            log::info!("store: passthrough unlock owned");
                            state.owned = true;
                        }
                    }
                }
                Some(e) => log::warn!("store: purchases: {e}"),
            },
            MSG_CHECKOUT => {
                state.checkout = Some(match error {
                    None => {
                        let sku = (api.sku)((api.purchase)(msg));
                        if !sku.is_null() && CStr::from_ptr(sku) == SKU {
                            log::info!("store: passthrough unlock bought");
                            state.owned = true;
                            crate::unlock::Status::Unlocked(String::new())
                        } else {
                            crate::unlock::Status::Failed("The store sold something else.".into())
                        }
                    }
                    Some(e) => {
                        log::info!("store: checkout ended: {e}");
                        // Closing the store's window is not a failure.
                        crate::unlock::Status::Failed("Cancelled".into())
                    }
                });
            }
            other => log::debug!("store: message {other:#x}"),
        }
    }
}

/// Whether the passthrough unlock is owned.
pub fn owns_unlock() -> bool {
    STATE.lock().unwrap().owned
}

/// Opens the store's checkout for the passthrough unlock.
pub fn checkout() {
    let mut state = STATE.lock().unwrap();
    let Some(api) = API.get() else {
        state.checkout = Some(crate::unlock::Status::Failed(
            "The Meta Quest Store isn't available.".into(),
        ));
        return;
    };
    state.checkout = Some(crate::unlock::Status::Starting);
    // SAFETY: a request on a started SDK; the answer comes through pump.
    unsafe { (api.checkout)(SKU.as_ptr()) };
}

/// Where the checkout is.
pub fn checkout_status() -> crate::unlock::Status {
    STATE
        .lock()
        .unwrap()
        .checkout
        .clone()
        .unwrap_or(crate::unlock::Status::Starting)
}
