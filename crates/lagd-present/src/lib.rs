//! `lagd-present` — a Vulkan layer that holds `vkQueuePresentKHR`.
//!
//! The layer is implicit and gated on `LAGD_PRESENT=1`, so it is not loaded
//! into every Vulkan process on the system; when it *is* loaded and the present
//! stage is at zero or dropped, the hook costs two relaxed atomic loads and a
//! hash lookup.
//!
//! # What this can and cannot delay
//!
//! Only Vulkan clients. OpenGL needs a different hook (`eglSwapBuffers` /
//! `glXSwapBuffers` via `LD_PRELOAD`), and nothing a layer can do from inside
//! a client process will delay the Wayland compositor's own output.
//!
//! # Why it also costs framerate
//!
//! Sleeping inside present pushes the frame's appearance back by the delay
//! *and* pushes back the start of the next frame, so throughput falls to
//! `1/(render + delay)`. Holding the frame without paying that would mean
//! owning the swapchain images, which a layer cannot do. For judging
//! perceptual latency this matters: a present delay is not a clean
//! manipulation the way the input delay is.

mod vk;

use std::collections::HashMap;
use std::ffi::{c_char, c_void, CStr};
use std::mem;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::thread;
use std::time::Duration;

use lagd_core::state::{Stage, StageId};

use vk::{
    find_chain_info, PFN_vkCreateDevice, PFN_vkCreateInstance, PFN_vkDestroyDevice,
    PFN_vkDestroyInstance, PFN_vkGetDeviceProcAddr, PFN_vkGetDeviceQueue, PFN_vkGetDeviceQueue2,
    PFN_vkGetInstanceProcAddr, PFN_vkQueuePresentKHR, PFN_vkVoidFunction, VkBaseInStructure,
    VkDevice, VkInstance, VkLayerDeviceCreateInfo, VkLayerInstanceCreateInfo,
    VkNegotiateLayerInterface, VkPhysicalDevice, VkQueue, VkResult,
    LAYER_NEGOTIATE_INTERFACE_STRUCT, VK_ERROR_INITIALIZATION_FAILED,
    VK_STRUCTURE_TYPE_LOADER_DEVICE_CREATE_INFO, VK_STRUCTURE_TYPE_LOADER_INSTANCE_CREATE_INFO,
    VK_SUCCESS,
};

/// Everything we need to call down the chain for one instance.
struct InstanceDispatch {
    get_instance_proc_addr: PFN_vkGetInstanceProcAddr,
    destroy_instance: Option<PFN_vkDestroyInstance>,
}

/// Everything we need to call down the chain for one device.
struct DeviceDispatch {
    get_device_proc_addr: PFN_vkGetDeviceProcAddr,
    destroy_device: Option<PFN_vkDestroyDevice>,
    queue_present: Option<PFN_vkQueuePresentKHR>,
    get_device_queue: Option<PFN_vkGetDeviceQueue>,
    get_device_queue2: Option<PFN_vkGetDeviceQueue2>,
}

type Registry<T> = RwLock<HashMap<usize, Arc<T>>>;

fn instances() -> &'static Registry<InstanceDispatch> {
    static R: OnceLock<Registry<InstanceDispatch>> = OnceLock::new();
    R.get_or_init(Registry::default)
}

fn devices() -> &'static Registry<DeviceDispatch> {
    static R: OnceLock<Registry<DeviceDispatch>> = OnceLock::new();
    R.get_or_init(Registry::default)
}

/// `vkQueuePresentKHR` takes a queue, not a device, so the queue's device
/// dispatch has to be recorded when the application asks for the queue. This is
/// why `vkGetDeviceQueue` and `vkGetDeviceQueue2` are hooked at all.
fn queues() -> &'static Registry<DeviceDispatch> {
    static R: OnceLock<Registry<DeviceDispatch>> = OnceLock::new();
    R.get_or_init(Registry::default)
}

/// The present stage's knobs, mapped once per process.
///
/// A failure to map is not fatal and must not be: a layer that cannot read its
/// config has to be transparent, not broken. It warns once and then reports no
/// delay forever.
fn present_stage() -> Option<&'static Stage> {
    static STAGE: OnceLock<Option<&'static Stage>> = OnceLock::new();
    *STAGE.get_or_init(|| match lagd_core::state::shared() {
        Ok(state) => Some(state.stage(StageId::Present)),
        Err(err) => {
            eprintln!("lagd-present: no control plane ({err}); this layer will not delay anything");
            None
        }
    })
}

/// Reads the delay to apply to the next present, or `None` for "do nothing".
fn present_delay() -> Option<Duration> {
    let delay = present_stage()?.effective()?;
    if delay.is_zero() {
        None
    } else {
        Some(delay)
    }
}

/// Reinterprets a typed entry point as the untyped pointer Vulkan hands back.
///
/// The caller recovers the real signature by the name it asked for, which is
/// the entire contract of `vkGetInstanceProcAddr`.
fn as_void_fn<T: Copy>(f: T) -> PFN_vkVoidFunction {
    const {
        assert!(
            mem::size_of::<T>() == mem::size_of::<PFN_vkVoidFunction>(),
            "entry points must be plain function pointers"
        );
    }
    // SAFETY: both sides are function pointers of equal size, and Vulkan's ABI
    // is defined in terms of exactly this cast.
    unsafe { mem::transmute_copy(&f) }
}

/// Resolves one entry point from the next layer down, by name.
///
/// # Safety
///
/// `gipa` must be the next layer's `vkGetInstanceProcAddr` and `instance` a
/// handle it accepts.
unsafe fn resolve_instance<T: Copy>(
    gipa: PFN_vkGetInstanceProcAddr,
    instance: VkInstance,
    name: &CStr,
) -> Option<T> {
    // SAFETY: delegated to the caller's contract.
    let f = unsafe { gipa(instance, name.as_ptr()) }?;
    // SAFETY: the loader guarantees the returned pointer has the signature
    // that belongs to `name`, which is what `T` names at every call site.
    Some(unsafe { mem::transmute_copy(&f) })
}

/// As [`resolve_instance`], for the device-level chain.
///
/// # Safety
///
/// `gdpa` must be the next layer's `vkGetDeviceProcAddr` and `device` a handle
/// it accepts.
unsafe fn resolve_device<T: Copy>(
    gdpa: PFN_vkGetDeviceProcAddr,
    device: VkDevice,
    name: &CStr,
) -> Option<T> {
    // SAFETY: delegated to the caller's contract.
    let f = unsafe { gdpa(device, name.as_ptr()) }?;
    // SAFETY: as in `resolve_instance`.
    Some(unsafe { mem::transmute_copy(&f) })
}

fn instance_dispatch(instance: VkInstance) -> Option<Arc<InstanceDispatch>> {
    instances().read().ok()?.get(&(instance as usize)).cloned()
}

fn device_dispatch(device: VkDevice) -> Option<Arc<DeviceDispatch>> {
    devices().read().ok()?.get(&(device as usize)).cloned()
}

fn queue_dispatch(queue: VkQueue) -> Option<Arc<DeviceDispatch>> {
    queues().read().ok()?.get(&(queue as usize)).cloned()
}

// ---------------------------------------------------------------------------
// Loader-facing exports
// ---------------------------------------------------------------------------

/// Negotiates the layer interface version with the loader.
///
/// # Safety
///
/// `p_version_struct` must point at a writable `VkNegotiateLayerInterface`.
#[no_mangle]
pub unsafe extern "system" fn lagd_vkNegotiateLoaderLayerInterfaceVersion(
    p_version_struct: *mut VkNegotiateLayerInterface,
) -> VkResult {
    if p_version_struct.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    // SAFETY: the caller guarantees the pointer is valid and writable.
    let v = unsafe { &mut *p_version_struct };
    if v.s_type != LAYER_NEGOTIATE_INTERFACE_STRUCT {
        return VK_ERROR_INITIALIZATION_FAILED;
    }

    // Speak the lower of the two versions, as the interface requires.
    v.loader_layer_interface_version = v.loader_layer_interface_version.min(2);
    v.pfn_get_instance_proc_addr = Some(get_instance_proc_addr);
    v.pfn_get_device_proc_addr = Some(get_device_proc_addr);
    v.pfn_get_physical_device_proc_addr = ptr::null_mut();
    VK_SUCCESS
}

/// The layer's `vkGetInstanceProcAddr`.
///
/// # Safety
///
/// `name` must be a valid NUL-terminated string and `instance` either null or
/// an instance this layer has seen created.
#[no_mangle]
pub unsafe extern "system" fn lagd_vkGetInstanceProcAddr(
    instance: VkInstance,
    name: *const c_char,
) -> PFN_vkVoidFunction {
    // SAFETY: delegated to the caller's contract.
    unsafe { get_instance_proc_addr(instance, name) }
}

/// The layer's `vkGetDeviceProcAddr`.
///
/// # Safety
///
/// `name` must be a valid NUL-terminated string and `device` a device this
/// layer has seen created.
#[no_mangle]
pub unsafe extern "system" fn lagd_vkGetDeviceProcAddr(
    device: VkDevice,
    name: *const c_char,
) -> PFN_vkVoidFunction {
    // SAFETY: delegated to the caller's contract.
    unsafe { get_device_proc_addr(device, name) }
}

// ---------------------------------------------------------------------------
// Chain
// ---------------------------------------------------------------------------

unsafe extern "system" fn get_instance_proc_addr(
    instance: VkInstance,
    name: *const c_char,
) -> PFN_vkVoidFunction {
    if name.is_null() {
        return None;
    }
    // SAFETY: the loader always passes a NUL-terminated entry point name.
    let name_bytes = unsafe { CStr::from_ptr(name) }.to_bytes();

    match name_bytes {
        b"vkGetInstanceProcAddr" => {
            return as_void_fn(get_instance_proc_addr as PFN_vkGetInstanceProcAddr)
        }
        b"vkCreateInstance" => return as_void_fn(create_instance as PFN_vkCreateInstance),
        b"vkDestroyInstance" => return as_void_fn(destroy_instance as PFN_vkDestroyInstance),
        b"vkCreateDevice" => return as_void_fn(create_device as PFN_vkCreateDevice),
        b"vkGetDeviceProcAddr" => {
            return as_void_fn(get_device_proc_addr as PFN_vkGetDeviceProcAddr)
        }
        _ => {}
    }

    // Everything else belongs to whoever is below us. Before the instance
    // exists there is no "below us" to ask, which is exactly why the names
    // above are answered unconditionally.
    let dispatch = instance_dispatch(instance)?;
    // SAFETY: `get_instance_proc_addr` came from the loader's link info and
    // `instance` is the handle it was recorded against.
    unsafe { (dispatch.get_instance_proc_addr)(instance, name) }
}

unsafe extern "system" fn get_device_proc_addr(
    device: VkDevice,
    name: *const c_char,
) -> PFN_vkVoidFunction {
    if name.is_null() {
        return None;
    }
    // SAFETY: as in `get_instance_proc_addr`.
    let name_bytes = unsafe { CStr::from_ptr(name) }.to_bytes();
    let dispatch = device_dispatch(device)?;

    match name_bytes {
        b"vkGetDeviceProcAddr" => as_void_fn(get_device_proc_addr as PFN_vkGetDeviceProcAddr),
        b"vkDestroyDevice" => as_void_fn(destroy_device as PFN_vkDestroyDevice),
        b"vkGetDeviceQueue" => as_void_fn(get_device_queue as PFN_vkGetDeviceQueue),
        b"vkGetDeviceQueue2" => as_void_fn(get_device_queue2 as PFN_vkGetDeviceQueue2),
        // Only claim present if the device below us really has it: an
        // application that probes for the entry point to decide whether it can
        // present must not get a false positive from us.
        b"vkQueuePresentKHR" if dispatch.queue_present.is_some() => {
            as_void_fn(queue_present as PFN_vkQueuePresentKHR)
        }
        _ => {
            // SAFETY: recorded from the loader's device link info, against
            // this device.
            unsafe { (dispatch.get_device_proc_addr)(device, name) }
        }
    }
}

unsafe extern "system" fn create_instance(
    p_create_info: *const VkBaseInStructure,
    p_allocator: *const c_void,
    p_instance: *mut VkInstance,
) -> VkResult {
    // SAFETY: the loader passes a valid create-info whose pNext chain holds
    // the instance link info.
    let chain = unsafe {
        find_chain_info(
            (*p_create_info).p_next,
            VK_STRUCTURE_TYPE_LOADER_INSTANCE_CREATE_INFO,
        )
    };
    if chain.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let chain = chain.cast::<VkLayerInstanceCreateInfo>().cast_mut();

    // SAFETY: `chain` points at the loader's own struct, and reading the union
    // member the loader just filled in is what the interface prescribes.
    let link = unsafe { (*chain).u.p_layer_info };
    if link.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    // SAFETY: the loader guarantees a valid link node here.
    let gipa = unsafe { (*link).pfn_next_get_instance_proc_addr };

    // Advancing the loader's link pointer is how a layer says "I am done with
    // the chain"; the layer below us reads the node we leave behind.
    unsafe {
        (*chain).u.p_layer_info = (*link).p_next;
    }

    // Per the loader interface, instance-level creation is resolved with a null
    // instance handle.
    // SAFETY: `gipa` is the next layer's entry point resolver.
    let next: Option<PFN_vkCreateInstance> =
        unsafe { resolve_instance(gipa, ptr::null_mut(), c"vkCreateInstance") };
    let Some(next_create) = next else {
        return VK_ERROR_INITIALIZATION_FAILED;
    };

    // SAFETY: forwarding the caller's own arguments unchanged.
    let result = unsafe { next_create(p_create_info, p_allocator, p_instance) };
    if result != VK_SUCCESS {
        return result;
    }

    // SAFETY: on success the loader has written a handle here.
    let instance = unsafe { *p_instance };
    // SAFETY: `gipa` accepts the instance it just helped create.
    let destroy_instance = unsafe { resolve_instance(gipa, instance, c"vkDestroyInstance") };

    if let Ok(mut map) = instances().write() {
        map.insert(
            instance as usize,
            Arc::new(InstanceDispatch {
                get_instance_proc_addr: gipa,
                destroy_instance,
            }),
        );
    }
    result
}

unsafe extern "system" fn destroy_instance(instance: VkInstance, p_allocator: *const c_void) {
    let dispatch = instances()
        .write()
        .ok()
        .and_then(|mut map| map.remove(&(instance as usize)));
    if let Some(destroy) = dispatch.and_then(|d| d.destroy_instance) {
        // SAFETY: resolved against this instance in `create_instance`.
        unsafe { destroy(instance, p_allocator) };
    }
}

// `gipa` and `gdpa` differ by one letter, and that is on purpose: they are the
// abbreviations the loader interface itself uses for the two resolvers, so
// anyone comparing this against the specification wants to read them unchanged.
#[allow(clippy::similar_names)]
unsafe extern "system" fn create_device(
    physical_device: VkPhysicalDevice,
    p_create_info: *const VkBaseInStructure,
    p_allocator: *const c_void,
    p_device: *mut VkDevice,
) -> VkResult {
    // SAFETY: the loader passes a valid create-info carrying the device link.
    let chain = unsafe {
        find_chain_info(
            (*p_create_info).p_next,
            VK_STRUCTURE_TYPE_LOADER_DEVICE_CREATE_INFO,
        )
    };
    if chain.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let chain = chain.cast::<VkLayerDeviceCreateInfo>().cast_mut();

    // SAFETY: reading the loader-filled union member, as the interface
    // prescribes.
    let link = unsafe { (*chain).u.p_layer_info };
    if link.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    // SAFETY: valid link node.
    let (gipa, gdpa) = unsafe {
        (
            (*link).pfn_next_get_instance_proc_addr,
            (*link).pfn_next_get_device_proc_addr,
        )
    };
    // SAFETY: advancing the chain for the layer below us.
    unsafe {
        (*chain).u.p_layer_info = (*link).p_next;
    }

    // `vkCreateDevice` is reached through the *instance* resolver with a null
    // handle: a layer is handed a VkPhysicalDevice here and has no instance to
    // pass, and the loader interface specifies this call shape for exactly
    // that reason.
    // SAFETY: `gipa` is the next layer's resolver.
    let next: Option<PFN_vkCreateDevice> =
        unsafe { resolve_instance(gipa, ptr::null_mut(), c"vkCreateDevice") };
    let Some(next_create) = next else {
        return VK_ERROR_INITIALIZATION_FAILED;
    };

    // SAFETY: forwarding the caller's arguments unchanged.
    let result = unsafe { next_create(physical_device, p_create_info, p_allocator, p_device) };
    if result != VK_SUCCESS {
        return result;
    }

    // SAFETY: on success the loader has written a handle here.
    let device = unsafe { *p_device };
    // SAFETY: `gdpa` accepts the device it just helped create.
    let dispatch = unsafe {
        DeviceDispatch {
            get_device_proc_addr: gdpa,
            destroy_device: resolve_device(gdpa, device, c"vkDestroyDevice"),
            queue_present: resolve_device(gdpa, device, c"vkQueuePresentKHR"),
            get_device_queue: resolve_device(gdpa, device, c"vkGetDeviceQueue"),
            get_device_queue2: resolve_device(gdpa, device, c"vkGetDeviceQueue2"),
        }
    };

    if let Ok(mut map) = devices().write() {
        map.insert(device as usize, Arc::new(dispatch));
    }
    result
}

unsafe extern "system" fn destroy_device(device: VkDevice, p_allocator: *const c_void) {
    let dispatch = devices()
        .write()
        .ok()
        .and_then(|mut map| map.remove(&(device as usize)));

    // Drop this device's queues too, or a later device that happens to reuse a
    // queue address would inherit a dangling dispatch table.
    if let Ok(mut map) = queues().write() {
        map.retain(|_, d| !matches!(&dispatch, Some(dead) if Arc::ptr_eq(d, dead)));
    }

    if let Some(destroy) = dispatch.and_then(|d| d.destroy_device) {
        // SAFETY: resolved against this device in `create_device`.
        unsafe { destroy(device, p_allocator) };
    }
}

unsafe extern "system" fn get_device_queue(
    device: VkDevice,
    family: u32,
    index: u32,
    p_queue: *mut VkQueue,
) {
    let Some(dispatch) = device_dispatch(device) else {
        return;
    };
    if let Some(get) = dispatch.get_device_queue {
        // SAFETY: resolved against this device; arguments forwarded unchanged.
        unsafe { get(device, family, index, p_queue) };
        // SAFETY: on return the driver has written the queue handle.
        unsafe { remember_queue(p_queue, &dispatch) };
    }
}

unsafe extern "system" fn get_device_queue2(
    device: VkDevice,
    p_queue_info: *const c_void,
    p_queue: *mut VkQueue,
) {
    let Some(dispatch) = device_dispatch(device) else {
        return;
    };
    if let Some(get) = dispatch.get_device_queue2 {
        // SAFETY: resolved against this device; arguments forwarded unchanged.
        unsafe { get(device, p_queue_info, p_queue) };
        // SAFETY: on return the driver has written the queue handle.
        unsafe { remember_queue(p_queue, &dispatch) };
    }
}

/// Records which device a queue belongs to, so `queue_present` can find its way
/// down the chain.
///
/// # Safety
///
/// `p_queue` must be null or point at a queue handle the driver has written.
unsafe fn remember_queue(p_queue: *mut VkQueue, dispatch: &Arc<DeviceDispatch>) {
    if p_queue.is_null() {
        return;
    }
    // SAFETY: delegated to the caller's contract.
    let queue = unsafe { *p_queue };
    if queue.is_null() {
        return;
    }
    if let Ok(mut map) = queues().write() {
        map.insert(queue as usize, Arc::clone(dispatch));
    }
}

unsafe extern "system" fn queue_present(queue: VkQueue, p_present_info: *const c_void) -> VkResult {
    let Some(dispatch) = queue_dispatch(queue) else {
        // A queue we never saw handed out. Failing the present would be worse
        // than not delaying it, but there is nothing to call down to.
        warn_once("present on an unknown queue; frame delay not applied");
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    let Some(present) = dispatch.queue_present else {
        return VK_ERROR_INITIALIZATION_FAILED;
    };

    if let Some(delay) = present_delay() {
        // Held before handing the frame down, so the image appears late rather
        // than the application being told late. See the module docs for why
        // this also costs throughput.
        thread::sleep(delay);
    }

    // SAFETY: resolved against this queue's device; arguments forwarded
    // unchanged.
    unsafe { present(queue, p_present_info) }
}

/// Diagnostics from inside someone else's render loop have to be rationed.
fn warn_once(message: &str) {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        eprintln!("lagd-present: {message}");
    }
}
