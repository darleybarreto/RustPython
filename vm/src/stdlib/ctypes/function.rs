// cspell:disable

use crate::builtins::{PyStr, PyStrRef, PyTuple, PyTupleRef, PyType, PyTypeRef, PyList, PyDictRef};
use crate::function::{PySetterValue, FuncArgs, OptionalArg}; 
use crate::slots::{PyTypeSlots, PyTpNew, PyTpGetattro, PyTpCall, ConstructorSlot}; 
use crate::common::rc::PyRc;
use crate::convert::ToPyObject;
use crate::stdlib::ctypes::PyCData;
use crate::stdlib::ctypes::array::PyCArray;
use crate::stdlib::ctypes::base::{PyCSimple, PyCDataApi, ffi_type_from_str, PyCArg, get_ffi_type};
use crate::types::{Callable, Constructor, PyTypeTypeUtils, DefaultPyObject}; 
use crate::{Py, PyRef, PyObjectPayload, PyThreadingConstraint, PyWeakRef, PyWeak, StaticType}; 
use crate::{PyObjectRef, PyResult, VirtualMachine, IntoPyObject, PyPayload, AsObject, TypeProtocol, BorrowValue};
use crossbeam_utils::atomic::AtomicCell;
use libffi::middle::{Abi, Arg, Cif, CodePtr, Type, Closure}; 
use libloading::Symbol;
use num_traits::ToPrimitive;
use rustpython_common::lock::PyRwLock;
use std::ffi::{c_void, CString};
use std::fmt::Debug;
use std::mem; 
use std::ptr;
use once_cell::sync::Lazy; 
use std::collections::HashMap; 


// https://github.com/python/cpython/blob/4f8bb3947cfbc20f970ff9d9531e1132a9e95396/Modules/_ctypes/callproc.c#L15

#[derive(Debug)]
pub struct Function {
    args: Vec<Type>,
    pointer: CodePtr,
    cif: Cif,
    ffi_return_type: Type, 
    original_restype: Option<PyObjectRef>, 
    abi: Abi, 
}

unsafe impl Send for Function {}
unsafe impl Sync for Function {}

type FP = unsafe extern "C" fn();

impl Function {
    pub unsafe fn load(
        library: &libloading::Library,
        function: &str,
        argtypes_opt: Option<PyObjectRef>, 
        restype_obj: Option<PyObjectRef>, 
        abi: Abi, 
        vm: &VirtualMachine,
    ) -> PyResult<Self> {
        let ffi_arg_types: Vec<Type> = match argtypes_opt {
            Some(argtypes_tuple_obj) if argtypes_tuple_obj.is_instance(&vm.ctx.types.tuple_type, vm) => {
                let argtypes_tuple = argtypes_tuple_obj.downcast_ref::<PyTuple>().unwrap();
                argtypes_tuple.iter().map(|ctypes_type_obj| {
                    // Attempt to get _type_ attribute, assuming it's a PyCSimpleType or compatible PyCData subtype
                    // This is a simplified version. Real implementation needs to handle various ctypes types
                    // (pointers, arrays, structures, etc.) and might involve calling a method like `_get_ffi_type_`
                    // on the ctypes_type_obj.
                    let type_char_obj = ctypes_type_obj.get_attr("_type_", vm)
                        .map_err(|_| vm.new_type_error(format!("argtype {:?} does not have a _type_ attribute", ctypes_type_obj)))?;
                    let type_char_str = type_char_obj.downcast_ref::<PyStr>()
                        .ok_or_else(|| vm.new_type_error(format!("_type_ attribute of argtype {:?} must be a string", ctypes_type_obj)))?;
                    
                    ffi_type_from_str(type_char_str.as_str())
                        .ok_or_else(|| vm.new_type_error(format!("Invalid _type_ string '{}' in argtypes", type_char_str.as_str())))
                }).collect::<PyResult<Vec<Type>>>()?
            }
            Some(_) if vm.is_none(&argtypes_tuple_obj) => { // _argtypes_ is explicitly None
                // Default behavior when _argtypes_ is None (e.g. could mean function is variadic, or infer from call)
                // For now, let's require _argtypes_ to be a tuple if provided and not None.
                // Or, if we want to support calling without _argtypes_ set (like original PoC):
                 vec![] // This would mean Cif::new might fail or use a default if not variadic
            }
            None => { // _argtypes_ field was None itself
                 vec![] // As above, Cif will be prepared with no specific arg types from Python side.
            }
            _ => { // _argtypes_ was set to something other than a tuple or None
                return Err(vm.new_type_error(
                    "_argtypes_ must be a tuple of ctypes types or None.".to_string()
                ));
            }
        };

        let terminated = format!("{}\0", function);
        let pointer: Symbol<'_, FP> = unsafe {
            library
                .get(terminated.as_bytes())
                .map_err(|err| err.to_string())
                .map_err(|err| vm.new_attribute_error(err))?
        };
        let code_ptr = CodePtr(*pointer as *mut _);

        let (determined_ffi_return_type, stored_original_restype) = match restype_obj.as_ref() {
            None | Some(obj) if vm.is_none(obj) => (Type::void(), None), // restype is None
            Some(obj) => {
                if let Ok(py_type) = obj.clone().downcast::<PyTypeRef>() { // It's a PyTypeRef
                    if py_type.is_subclass(PyCData::class(&vm.ctx).as_ref(), vm) { // And a ctypes type
                        let type_char_obj = py_type.get_attr("_type_", vm)
                            .map_err(|_| vm.new_type_error(format!("ctypes type {:?} as restype does not have a _type_ attribute", py_type)))?;
                        let type_char_str = type_char_obj.downcast_ref::<PyStr>()
                            .ok_or_else(|| vm.new_type_error(format!("_type_ attribute of restype {:?} must be a string", py_type)))?;
                        
                        let ffi_type = ffi_type_from_str(type_char_str.as_str())
                            .ok_or_else(|| vm.new_type_error(format!("Invalid _type_ string '{}' in restype", type_char_str.as_str())))?;
                        (ffi_type, Some(obj.clone()))
                    } else if obj.is_callable(vm) { // A non-ctypes PyTypeRef that is callable (e.g. type itself if it's a callable type)
                        (Type::c_int(), Some(obj.clone()))
                    } else {
                        return Err(vm.new_type_error(
                           format!("restype is a type but not a ctypes type or callable: {:?}", obj.class().name())
                        ));
                    }
                } else if obj.is_callable(vm) { // Not a PyTypeRef, but is callable (e.g. Python function)
                    (Type::c_int(), Some(obj.clone()))
                } else {
                    return Err(vm.new_type_error(
                        format!("restype must be a ctypes type, a callable, or None, not {:?}", obj.class().name())
                    ));
                }
            }
        };
        
        let cif = Cif::new(ffi_arg_types.clone(), determined_ffi_return_type.clone());
        Ok(Function {
            args: ffi_arg_types,
            cif,
            pointer: code_ptr,
            ffi_return_type: determined_ffi_return_type,
            original_restype: stored_original_restype,
        })
    }

    pub unsafe fn call(
        &self,
        args: Vec<PyObjectRef>, // These are Python arguments passed to the function
        vm: &VirtualMachine,
    ) -> PyResult<PyObjectRef> {
        let mut ffi_args = Vec::with_capacity(self.args.len());
        if args.len() != self.args.len() {
            return Err(vm.new_type_error(format!(
                "Expected {} arguments (based on _argtypes_), got {}",
                self.args.len(),
                args.len()
            )));
        }

        for (py_arg, ffi_type_expected) in args.iter().zip(self.args.iter()) {
            // Argument conversion logic - Placeholder, needs robust implementation
            if let Some(simple_arg) = py_arg.payload_if_subclass::<PyCSimple>(vm) {
                 ffi_args.push(simple_arg.to_arg(ffi_type_expected.clone(), vm)?);
            } else if let Some(array_arg) = py_arg.payload_if_subclass::<PyCArray>(vm) {
                 ffi_args.push(array_arg.to_arg(vm)?);
            }
            // TODO: Add more types like pointers, String/Bytes for char*, etc.
            else {
                return Err(vm.new_type_error(format!(
                    "Argument type {:?} not yet supported for FFI call to convert to {:?}",
                    py_arg.class().name(), ffi_type_expected
                )));
            }
        }
        
        let result_val = match self.original_restype.as_ref() {
            None => { // Corresponds to Type::void() or restype explicitly set to None
                self.cif.call::<()>(self.pointer, &ffi_args);
                vm.ctx.none()
            }
            Some(original_restype_obj) => {
                let is_ctypes_type = if let Ok(py_type) = original_restype_obj.clone().downcast::<PyTypeRef>() {
                    py_type.is_subclass(PyCData::class(&vm.ctx).as_ref(), vm)
                } else { false };

                if is_ctypes_type {
                    // Assume simple types for now, based on _type_ char.
                    // More complex types (pointers, structures) would need more handling here.
                    let type_char_obj = original_restype_obj.get_attr("_type_", vm)?;
                    let type_char = type_char_obj.downcast_ref::<PyStr>().unwrap().as_str();

                    match type_char {
                        "i" | "l" => vm.ctx.new_int(self.cif.call::<i32>(self.pointer, &ffi_args)).into(),
                        "I" | "L" => vm.ctx.new_int(self.cif.call::<u32>(self.pointer, &ffi_args)).into(),
                        "q" => vm.ctx.new_int(self.cif.call::<i64>(self.pointer, &ffi_args)).into(),
                        "Q" => vm.ctx.new_int(self.cif.call::<u64>(self.pointer, &ffi_args)).into(),
                        "b" => vm.ctx.new_int(self.cif.call::<i8>(self.pointer, &ffi_args)).into(),
                        "B" => vm.ctx.new_int(self.cif.call::<u8>(self.pointer, &ffi_args)).into(),
                        "h" => vm.ctx.new_int(self.cif.call::<i16>(self.pointer, &ffi_args)).into(),
                        "H" => vm.ctx.new_int(self.cif.call::<u16>(self.pointer, &ffi_args)).into(),
                        "f" => vm.ctx.new_float(self.cif.call::<f32>(self.pointer, &ffi_args) as f64).into(),
                        "d" => vm.ctx.new_float(self.cif.call::<f64>(self.pointer, &ffi_args)).into(),
                        "?" => vm.ctx.new_bool(self.cif.call::<u8>(self.pointer, &ffi_args) != 0).into(),
                        "P" => { // c_void_p
                             let ptr_result = self.cif.call::<*mut std::ffi::c_void>(self.pointer, &ffi_args);
                             if ptr_result.is_null() { vm.ctx.none() } else { vm.ctx.new_int(ptr_result as usize).into() }
                        }
                        "z" => { // c_char_p
                            let ptr_result = self.cif.call::<*mut std::ffi::c_char>(self.pointer, &ffi_args);
                            if ptr_result.is_null() {
                                vm.ctx.none()
                            } else {
                                let c_str = std::ffi::CStr::from_ptr(ptr_result);
                                vm.ctx.new_bytes(c_str.to_bytes().to_vec()).into()
                            }
                        }
                        // "Z" => { // c_wchar_p - TODO: Requires knowing wchar_t size and proper conversion
                        //    return Err(vm.new_not_implemented_error("c_wchar_p restype not implemented".to_string()));
                        // }
                        _ => return Err(vm.new_type_error(format!("Unsupported _type_ string '{}' in restype for result conversion", type_char))),
                    }
                } else if original_restype_obj.is_callable(vm) {
                    // Assumed FFI return is c_int for this case.
                    let raw_int_result = self.cif.call::<i32>(self.pointer, &ffi_args);
                    let py_int_result = vm.ctx.new_int(raw_int_result).into();
                    original_restype_obj.call((py_int_result,), vm)?
                } else {
                     return Err(vm.new_type_error(format!("Invalid original_restype ({:?}) found during call", original_restype_obj.class().name())));
                }
            }
        };
        Ok(result_val)
    }
}

#[pyclass(module = "_ctypes", name = "CFuncPtr", base = "PyCData")]
#[derive(PyPayload)]
pub struct PyCFuncPtr {
    pub name: PyRwLock<String>,
    pub _flags_: AtomicCell<u32>,
    pub _restype_: PyRwLock<Option<PyObjectRef>>, // Changed from PyTypeRef
    // For CFUNCTYPE, this handler is a Python callable.
    // For CDLL-returned functions, this could store library identifier or be unused if lib retrieved globally.
    pub handler: PyObjectRef, 
    pub abi: PyRwLock<Abi>,
    // Add a field to store library name, used by functions from CDLL
    pub library_name: Option<String>,
    pub _argtypes_: PyRwLock<Option<PyObjectRef>>,
    pub _errcheck_: PyRwLock<Option<PyObjectRef>>, // Added _errcheck_ field
}

impl Debug for PyCFuncPtr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PyCFuncPtr")
            .field("name", &self.name)
            .field("abi", &self.abi.read())
            .field("library_name", &self.library_name)
            .field("_argtypes_", &self._argtypes_.read())
            .finish()
    }
}

impl PyCFuncPtr {
    // Constructor for functions obtained from CDLL
    pub(crate) fn new_for_dll(
        name: PyStrRef,
        library_name: String,
        abi: Abi,
        vm: &VirtualMachine,
    ) -> PyResult {
        Ok(PyCFuncPtr {
            name: PyRwLock::new(name.as_str().to_owned()),
            _flags_: AtomicCell::new(0), // Default flags
            _restype_: PyRwLock::new(None), // Default restype
            // For DLL functions, handler might not be a Python callable in the same way.
            // Store library_name and use a placeholder or vm.ctx.none() for handler if not applicable.
            handler: vm.ctx.none(), 
            abi: PyRwLock::new(abi),
            library_name: Some(library_name),
            _argtypes_: PyRwLock::new(None),
            _errcheck_: PyRwLock::new(None), // Initialize _errcheck_
        }
        .into_pyobject(vm))
    }
}

impl Constructor for PyCFuncPtr {
    type Args = (PyTupleRef, FuncArgs); // (name, handler_obj, ?restype, ?flags) for CFUNCTYPE

    fn py_new(cls: PyTypeRef, (tuple, args_after_tuple): Self::Args, vm: &VirtualMachine) -> PyResult {
        let elements = tuple.as_slice();
        if elements.len() < 2 {
            return Err(vm.new_type_error(
                "CFuncPtr constructor needs at least (name, callable), or use from_address".to_string()
            ));
        }

        let name_obj = &elements[0];
        let handler = elements[1].clone(); // This is the callable for CFUNCTYPE

        // TODO: Parse other arguments like restype, flags if provided for CFUNCTYPE
        // For now, simplified:
        let name = name_obj.downcast_ref::<PyStr>()
            .ok_or_else(|| vm.new_type_error("First argument must be a string (function name)".to_string()))?
            .as_str()
            .to_owned();
        
        // Default ABI for CFUNCTYPE, usually CDECL unless specified otherwise
        let default_abi = Abi::Cdecl; 
        // Potentially parse flags from args_after_tuple to change ABI if needed for CFUNCTYPE

        Ok(Self {
            name: PyRwLock::new(name),
            _flags_: AtomicCell::new(0), // Initialize flags, could be parsed from args
            _restype_: PyRwLock::new(None), // Initialize restype, could be parsed from args
            handler, // For CFUNCTYPE, this is the Python callable
            abi: PyRwLock::new(default_abi),
            library_name: None, // Not from a DLL
            _argtypes_: PyRwLock::new(None),
            _errcheck_: PyRwLock::new(None), // Initialize _errcheck_
        }
        .to_pyobject(vm))
    }
}

impl Callable for PyCFuncPtr {
    type Args = FuncArgs;
    fn call(zelf: &Py<Self>, args: Self::Args, vm: &VirtualMachine) -> PyResult {
        unsafe {
            // Distinguish between CFUNCTYPE call (handler is Python callable) 
            // and CDLL function call (handler is none, load from library_name)
            if zelf.library_name.is_some() {
                // This is a function from a CDLL object
                let lib_name = zelf.library_name.as_ref().unwrap(); // Safe due to check
                
                // Access the global LIBCACHE from loaders.rs (need to make it accessible, or pass vm.stdlib_ctypes_libcache)
                // For now, assuming direct access or a helper function to get LIBCACHE.
                // This part needs careful handling of LIBCACHE visibility.
                // Let's assume vm has a way to get to LIBCACHE for now.
                // This is a simplified placeholder for library loading:
                let library_cache_static = &super::loaders::LIBCACHE; // This is a placeholder, proper access TBD
                let lib_cache_read_guard = library_cache_static.read();
                let library = lib_cache_read_guard.get(lib_name)
                    .ok_or_else(|| vm.new_os_error(format!("Library {} not found or unloaded", lib_name)))?;

                // Now use `library` (which is a libloading::Library)
                let name = zelf.name.read();
                let restype_obj = zelf._restype_.read().clone(); 
                let argtypes_opt = zelf._argtypes_.read().clone();
                let abi_val = zelf.abi.read().clone(); // Read ABI value

                let func = Function::load(
                    library, 
                    &name,
                    argtypes_opt, 
                    restype_obj,  
                    abi_val, // Pass ABI value to Function::load
                    vm,
                )?;
                // func.args (Type vector) should now be derived from _argtypes_ if it was provided.
                // func.call will use this to validate/convert runtime Python args.
                let raw_py_result = func.call(args.args.clone(), vm)?; // args.args are the runtime Python arguments

                if let Some(errcheck_callable) = zelf._errcheck_.read().clone() {
                    let py_args_tuple = vm.ctx.new_tuple(args.args);
                    // The errcheck function is called with three arguments:
                    // result, func, arguments
                    //   result: the result from the C function call
                    //   func: the CFuncPtr object itself
                    //   arguments: the original tuple of arguments passed to the function call
                    vm.invoke(&errcheck_callable, (raw_py_result, zelf.as_object().clone(), py_args_tuple))
                } else {
                    Ok(raw_py_result)
                }
            } else {
                // This is a CFUNCTYPE (handler is a Python callable)
                // The existing logic for CFUNCTYPE would go here, calling `zelf.handler`
                // This part is complex and involves creating a Cif and calling the Python handler.
                // For now, returning NotImplementedError for CFUNCTYPE calls.
                Err(vm.new_not_implemented_error(
                    "Calling CFUNCTYPE instances not fully implemented here yet".to_string()
                ))
            }
        }
    }
}


// #################################################################
// ## CFUNCTYPE related structures and implementation (STRUCTS ONLY)
// #################################################################

#[pyclass(name = "PyCFuncType_Type", module = "_ctypes", base = "PyType")]
#[derive(Debug, PyPayload)]
pub struct PyCFuncTypeType;

impl PyTypeSlots for PyCFuncTypeType {
    // Default behavior for a metaclass.
}
impl DefaultPyObject for PyCFuncTypeType {}


#[derive(Debug, Clone)]
pub struct PyCallbackSignature {
    pub python_restype: PyObjectRef,
    pub python_argtypes: PyObjectRef, // Python tuple of Python types
    pub ffi_argtypes: Vec<libffi::middle::Type>,
    pub ffi_restype: libffi::middle::Type,
    pub abi: libffi::middle::Abi,
    // TODO: flags like use_errno, use_last_error
}

#[pyclass(name = "PyCallback", module = "_ctypes", base = "PyCData", with(Constructor))]
#[derive(Debug, PyPayload)]
pub struct PyCallbackObject {
    pub callable: PyObjectRef, // The user's Python function
    pub signature: Option<PyCallbackSignature>, // The signature it was created with
    // TODO: closure: Option<libffi::middle::Closure<'static>>,
    // TODO: address: usize,
}

impl Constructor for PyCallbackObject {
    type Args = PyObjectRef; // Expects the Python callable

    fn py_new(cls: PyTypeRef, callable_arg: Self::Args, vm: &VirtualMachine) -> PyResult {
        // Basic validation: Check if callable_arg is actually callable.
        if !callable_arg.is_callable(vm) {
            return Err(vm.new_type_error("Argument must be a callable".to_string()));
        }
        // TODO: In a later step (6.D), this is where we'd retrieve the PyCallbackSignature
        // from `cls.payload()` or similar, once CFUNCTYPE sets it up.
        // For now, initialize with a placeholder or None for signature.
        let instance = PyCallbackObject {
            callable: callable_arg,
            signature: None, // Placeholder
            // closure: None, // Placeholder
            // address: 0,    // Placeholder
        };
        instance.into_pyobject_with_type(vm, cls)
    }
}
impl DefaultPyObject for PyCallbackObject {} // Needed if no custom constructor for PyCallbackObject itself if not for `with(Constructor)`

// Add init_types if it's not already there, or modify existing one
// Assuming init_types from previous tasks exists and needs modification.
// If it doesn't exist, it should be created like:
// pub(super) fn init_types(context: &crate::Context) {
//     PyCFuncPtr::init_type(context);
//     PyCFuncTypeType::init_type(context);
//     PyCallbackObject::init_type(context);
// }
// For now, I will assume it exists from previous steps and just add to it,
// or create it if it's missing.
// A common pattern in other ctypes files is `extend_class` called within `init_types`
// pub(super) fn init_types(vm: &VirtualMachine, module: &PyObjectRef) {
//    PyCFuncPtr::extend_class(&vm.ctx, PyCFuncPtr::static_type().as_ref());
// }
// Based on the task, it seems to be:
// PyCFuncTypeType::init_type(context);
// PyCallbackObject::init_type(context);
// Let's ensure it's integrated correctly.
// The `init_types` in this file seems to take `vm` and `module`.
// `extend_class` is more common for PyTypeRef setup.
// For now, let's modify the existing init_types.

// The existing init_types function:
// pub(super) fn init_types(vm: &VirtualMachine, module: &PyObjectRef) {
//    PyCFuncPtr::extend_class(&vm.ctx, PyCFuncPtr::static_type().as_ref());
// }
// We need to add to this.

#[pyclass(flags(BASETYPE), with(Callable, Constructor))]
impl PyCFuncPtr {
    // existing PyCFuncPtr methods...
    #[pygetset(magic)]
    fn name(&self) -> String {
        self.name.read().clone()
    }

    #[pygetset(setter, magic)]
    fn set_name(&self, name: String) {
        *self.name.write() = name;
    }

    #[pygetset(name = "_restype_")]
    fn restype(&self, vm: &VirtualMachine) -> PyObjectRef { // Changed return type & signature
        self._restype_
            .read()
            .as_ref()
            .cloned()
            .unwrap_or_else(|| vm.ctx.none())
    }

    #[pygetset(name = "_restype_", setter)]
    fn set_restype(&self, restype: PyObjectRef, vm: &VirtualMachine) { // Changed parameter type & signature
        // CPython allows setting restype to None, a ctypes type, or a callable.
        if vm.is_none(&restype) {
            *self._restype_.write() = None;
        } else {
            *self._restype_.write() = Some(restype);
        }
    }

    // Add methods to get/set ABI if needed, or handle through flags
    #[pygetset(name = "_abi_")]
    fn get_abi(&self, _vm: &VirtualMachine) -> PyResult<String> {
        match *self.abi.read() {
            Abi::Cdecl => Ok("cdecl".to_string()),
            Abi::Stdcall => Ok("stdcall".to_string()),
            Abi::Default => Ok("default".to_string()), 
            // Consider adding more specific ABI names if they become relevant
            // for ctypes usage in RustPython (e.g., Fastcall, SystemV).
            // For now, "unknown" covers other specific but less common ABIs.
            _ => Ok("unknown".to_string()), 
        }
    }

    #[pygetset(name = "_argtypes_")]
    fn argtypes(&self, vm: &VirtualMachine) -> PyObjectRef {
        self._argtypes_
            .read()
            .as_ref()
            .cloned()
            .unwrap_or_else(|| vm.ctx.none())
    }

    #[pygetset(name = "_argtypes_", setter)]
    fn set_argtypes(&self, value: PyObjectRef, vm: &VirtualMachine) -> PyResult<()> {
        if vm.is_none(&value) {
            *self._argtypes_.write() = None;
            return Ok(());
        }
        // CPython allows setting argtypes to a list or tuple.
        // We'll require a tuple for now, or convert list to tuple.
        // For simplicity, let's expect a tuple.
        
        let tuple_val = if value.is_instance(&vm.ctx.types.tuple_type, vm) {
            value
        } else if value.is_instance(&vm.ctx.types.list_type, vm) {
            let list_obj = value.downcast_ref::<PyList>().unwrap();
            vm.ctx.new_tuple(list_obj.borrow_vec().to_vec())
        } else {
            return Err(vm.new_type_error("argtypes must be a tuple, list, or None".to_string()));
        };

        // Optional: Validate elements of the tuple are valid ctypes type objects.
        // This could involve checking if each element is a PyTypeRef that is a subclass of PyCData,
        // or has a _type_ attribute, etc.
        // For now, just storing the tuple is acceptable as per subtask.
        *self._argtypes_.write() = Some(tuple_val);
        Ok(())
    }

    #[pygetset(name = "_errcheck_")]
    fn get_errcheck(&self, vm: &VirtualMachine) -> PyObjectRef {
        self._errcheck_
            .read()
            .as_ref()
            .cloned()
            .unwrap_or_else(|| vm.ctx.none())
    }

    #[pygetset(name = "_errcheck_", setter)]
    fn set_errcheck(&self, value: PyObjectRef, vm: &VirtualMachine) -> PyResult<()> {
        if vm.is_none(&value) {
            *self._errcheck_.write() = None;
            return Ok(());
        }
        if value.is_callable(vm) {
            *self._errcheck_.write() = Some(value);
            Ok(())
        } else {
            Err(vm.new_type_error("errcheck must be callable or None".to_string()))
        }
    }
}


pub(super) fn init_types(vm: &VirtualMachine, module: &PyObjectRef) { // Modified
    PyCFuncPtr::extend_class(&vm.ctx, PyCFuncPtr::static_type().as_ref());
    PyCFuncTypeType::extend_class(&vm.ctx, PyCFuncTypeType::static_type().as_ref());
    PyCallbackObject::extend_class(&vm.ctx, PyCallbackObject::static_type().as_ref());
}
                &res_type,
                vm,
            )?;
            func.call(args.args, vm)
        }
    }
}

#[pyclass(flags(BASETYPE), with(Callable, Constructor))]
impl PyCFuncPtr {
    #[pygetset(magic)]
    fn name(&self) -> String {
        self.name.read().clone()
    }

    #[pygetset(setter, magic)]
    fn set_name(&self, name: String) {
        *self.name.write() = name;
    }

    #[pygetset(name = "_restype_")]
    fn restype(&self) -> Option<PyTypeRef> {
        self._restype_.read().as_ref().cloned()
    }

    #[pygetset(name = "_restype_", setter)]
    fn set_restype(&self, restype: PyTypeRef) {
        *self._restype_.write() = Some(restype);
    }
}
