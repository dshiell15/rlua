use std::any::Any;
use std::borrow::Cow;
use std::fmt::Write;
use std::os::raw::{c_char, c_int, c_void};
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::sync::Arc;
use std::{mem, ptr, slice};

use crate::error::{Error, Result};
use crate::ffi;

// Checks that Lua has enough free stack space for future stack operations.  On failure, this will
// panic with an internal error message.
pub unsafe fn assert_stack(state: *mut ffi::lua_State, amount: c_int) {
    // TODO: This should only be triggered when there is a logic error in `rlua`.  In the future,
    // when there is a way to be confident about stack safety and test it, this could be enabled
    // only when `cfg!(debug_assertions)` is true.
    rlua_assert!(
        ffi::lua_checkstack(state, amount) != 0,
        "out of stack space"
    );
}

// Checks that Lua has enough free stakc space and returns `Error::StackError` on failure.
pub unsafe fn check_stack(state: *mut ffi::lua_State, amount: c_int) -> Result<()> {
    if ffi::lua_checkstack(state, amount) == 0 {
        Err(Error::StackError)
    } else {
        Ok(())
    }
}

pub struct StackGuard {
    state: *mut ffi::lua_State,
    top: c_int,
}

impl StackGuard {
    // Creates a StackGuard instance with wa record of the stack size, and on Drop will check the
    // stack size and drop any extra elements.  If the stack size at the end is *smaller* than at
    // the beginning, this is considered a fatal logic error and will result in a panic.
    pub unsafe fn new(state: *mut ffi::lua_State) -> StackGuard {
        StackGuard {
            state,
            top: ffi::lua_gettop(state),
        }
    }
}

impl Drop for StackGuard {
    fn drop(&mut self) {
        unsafe {
            let top = ffi::lua_gettop(self.state);
            if top > self.top {
                ffi::lua_settop(self.state, self.top);
            } else if top < self.top {
                rlua_panic!("{} too many stack values popped", self.top - top);
            }
        }
    }
}

// Call a function that calls into the Lua API and may trigger a Lua error (longjmp) in a safe way.
// Wraps the inner function in a call to `lua_pcall`, so the inner function only has access to a
// limited lua stack.  `nargs` is the same as the the parameter to `lua_pcall`, and `nresults` is
// always LUA_MULTRET.  Internally uses 2 extra stack spaces, and does not call checkstack.
// Provided function must *never* panic.
pub unsafe fn protect_lua(
    state: *mut ffi::lua_State,
    nargs: c_int,
    f: unsafe extern "C" fn(*mut ffi::lua_State) -> c_int,
) -> Result<()> {
    let stack_start = ffi::lua_gettop(state) - nargs;

    ffi::lua_pushcfunction(state, error_traceback);
    ffi::lua_pushcfunction(state, f);
    if nargs > 0 {
        ffi::lua_rotate(state, stack_start + 1, 2);
    }

    let ret = ffi::lua_pcall(state, nargs, ffi::LUA_MULTRET, stack_start + 1);
    ffi::lua_remove(state, stack_start + 1);

    if ret == ffi::LUA_OK {
        Ok(())
    } else {
        Err(pop_error(state, ret))
    }
}

// Call a function that calls into the Lua API and may trigger a Lua error (longjmp) in a safe way.
// Wraps the inner function in a call to `lua_pcall`, so the inner function only has access to a
// limited lua stack.  `nargs` and `nresults` are similar to the parameters of `lua_pcall`, but the
// given function return type is not the return value count, instead the inner function return
// values are assumed to match the `nresults` param.  Internally uses 3 extra stack spaces, and does
// not call checkstack.  Provided function must *not* panic, and since it will generally be
// lonjmping, should not contain any values that implement Drop.
pub unsafe fn protect_lua_closure<F, R>(
    state: *mut ffi::lua_State,
    nargs: c_int,
    nresults: c_int,
    f: F,
) -> Result<R>
where
    F: Fn(*mut ffi::lua_State) -> R,
    R: Copy,
{
    union URes<R: Copy> {
        uninit: (),
        init: R,
    }

    struct Params<F, R: Copy> {
        function: F,
        result: URes<R>,
        nresults: c_int,
    }

    unsafe extern "C" fn do_call<F, R>(state: *mut ffi::lua_State) -> c_int
    where
        R: Copy,
        F: Fn(*mut ffi::lua_State) -> R,
    {
        let params = ffi::lua_touserdata(state, -1) as *mut Params<F, R>;
        ffi::lua_pop(state, 1);

        (*params).result.init = ((*params).function)(state);

        if (*params).nresults == ffi::LUA_MULTRET {
            ffi::lua_gettop(state)
        } else {
            (*params).nresults
        }
    }

    let stack_start = ffi::lua_gettop(state) - nargs;

    ffi::lua_pushcfunction(state, error_traceback);
    ffi::lua_pushcfunction(state, do_call::<F, R>);
    if nargs > 0 {
        ffi::lua_rotate(state, stack_start + 1, 2);
    }

    let mut params = Params {
        function: f,
        result: URes { uninit: () },
        nresults,
    };

    ffi::lua_pushlightuserdata(state, &mut params as *mut Params<F, R> as *mut c_void);
    let ret = ffi::lua_pcall(state, nargs + 1, nresults, stack_start + 1);
    ffi::lua_remove(state, stack_start + 1);

    if ret == ffi::LUA_OK {
        // LUA_OK is only returned when the do_call function has completed successfully, so
        // params.result is definitely initialized.
        Ok(params.result.init)
    } else {
        Err(pop_error(state, ret))
    }
}

// Pops an error off of the stack and returns it.  The specific behavior depends on the type of the
// error at the top of the stack:
//   1) If the error is actually a WrappedPanic, this will continue the panic.
//   2) If the error on the top of the stack is actually a WrappedError, just returns it.
//   3) Otherwise, interprets the error as the appropriate lua error.
// Uses 2 stack spaces, does not call lua_checkstack.
pub unsafe fn pop_error(state: *mut ffi::lua_State, err_code: c_int) -> Error {
    rlua_debug_assert!(
        err_code != ffi::LUA_OK && err_code != ffi::LUA_YIELD,
        "pop_error called with non-error return code"
    );

    if let Some(err) = get_wrapped_error(state, -1).as_ref() {
        ffi::lua_pop(state, 1);
        err.clone()
    } else if is_wrapped_panic(state, -1) {
        let panic = get_userdata::<WrappedPanic>(state, -1);
        if let Some(p) = (*panic).0.take() {
            resume_unwind(p);
        } else {
            rlua_panic!("error during panic handling, panic was resumed twice")
        }
    } else {
        let err_string = to_string(state, -1).into_owned();
        ffi::lua_pop(state, 1);

        match err_code {
            ffi::LUA_ERRRUN => Error::RuntimeError(err_string),
            ffi::LUA_ERRSYNTAX => {
                Error::SyntaxError {
                    // This seems terrible, but as far as I can tell, this is exactly what the
                    // stock Lua REPL does.
                    incomplete_input: err_string.ends_with("<eof>"),
                    message: err_string,
                }
            }
            ffi::LUA_ERRERR => {
                // This error is raised when the error handler raises an error too many times
                // recursively, and continuing to trigger the error handler would cause a stack
                // overflow.  It is not very useful to differentiate between this and "ordinary"
                // runtime errors, so we handle them the same way.
                Error::RuntimeError(err_string)
            }
            ffi::LUA_ERRMEM => Error::MemoryError(err_string),
            _ => rlua_panic!("unrecognized lua error code"),
        }
    }
}

// Internally uses 4 stack spaces, does not call checkstack
pub unsafe fn push_string<S: ?Sized + AsRef<[u8]>>(
    state: *mut ffi::lua_State,
    s: &S,
) -> Result<()> {
    protect_lua_closure(state, 0, 1, |state| {
        let s = s.as_ref();
        ffi::lua_pushlstring(state, s.as_ptr() as *const c_char, s.len());
    })
}

// Internally uses 4 stack spaces, does not call checkstack
pub unsafe fn push_userdata_uv<T>(
    state: *mut ffi::lua_State,
    t: T,
    uvalues_count: c_int,
) -> Result<()> {
    rlua_debug_assert!(
        uvalues_count > 0,
        "userdata user values cannot be below zero"
    );
    let ud = protect_lua_closure(state, 0, 1, move |state| {
        ffi::lua_newuserdatauv(state, mem::size_of::<T>(), uvalues_count) as *mut T
    })?;
    ptr::write(ud, t);
    Ok(())
}

pub unsafe fn get_userdata<T>(state: *mut ffi::lua_State, index: c_int) -> *mut T {
    let ud = ffi::lua_touserdata(state, index) as *mut T;
    rlua_debug_assert!(!ud.is_null(), "userdata pointer is null");
    ud
}

// Pops the userdata off of the top of the stack and returns it to rust, invalidating the lua
// userdata and gives it the special "destructed" userdata metatable.  Userdata must not have been
// previously invalidated, and this method does not check for this.  Uses 1 extra stack space and
// does not call checkstack
pub unsafe fn take_userdata<T>(state: *mut ffi::lua_State) -> T {
    // We set the metatable of userdata on __gc to a special table with no __gc method and with
    // metamethods that trigger an error on access.  We do this so that it will not be double
    // dropped, and also so that it cannot be used or identified as any particular userdata type
    // after the first call to __gc.
    get_destructed_userdata_metatable(state);
    ffi::lua_setmetatable(state, -2);
    let ud = ffi::lua_touserdata(state, -1) as *mut T;
    rlua_debug_assert!(!ud.is_null(), "userdata pointer is null");
    ffi::lua_pop(state, 1);
    ptr::read(ud)
}

// Populates the given table with the appropriate members to be a userdata metatable for the given
// type.  This function takes the given table at the `metatable` index, and adds an appropriate __gc
// member to it for the given type and a __metatable entry to protect the table from script access.
// The function also, if given a `members` table index, will set up an __index metamethod to return
// the appropriate member on __index.  Additionally, if there is already an __index entry on the
// given metatable, instead of simply overwriting the __index, instead the created __index method
// will capture the previous one, and use it as a fallback only if the given key is not found in the
// provided members table.  Internally uses 6 stack spaces and does not call checkstack.
pub unsafe fn init_userdata_metatable<T>(
    state: *mut ffi::lua_State,
    metatable: c_int,
    members: Option<c_int>,
) -> Result<()> {
    // Used if both an __index metamethod is set and regular methods, checks methods table
    // first, then __index metamethod.
    unsafe extern "C" fn meta_index_impl(state: *mut ffi::lua_State) -> c_int {
        ffi::luaL_checkstack(state, 2, ptr::null());

        ffi::lua_pushvalue(state, -1);
        ffi::lua_gettable(state, ffi::lua_upvalueindex(2));
        if ffi::lua_isnil(state, -1) == 0 {
            ffi::lua_insert(state, -3);
            ffi::lua_pop(state, 2);
            1
        } else {
            ffi::lua_pop(state, 1);
            ffi::lua_pushvalue(state, ffi::lua_upvalueindex(1));
            ffi::lua_insert(state, -3);
            ffi::lua_call(state, 2, 1);
            1
        }
    }

    let members = members.map(|i| ffi::lua_absindex(state, i));
    ffi::lua_pushvalue(state, metatable);

    if let Some(members) = members {
        push_string(state, "__index")?;
        ffi::lua_pushvalue(state, -1);

        let index_type = ffi::lua_rawget(state, -3);
        if index_type == ffi::LUA_TNIL {
            ffi::lua_pop(state, 1);
            ffi::lua_pushvalue(state, members);
        } else if index_type == ffi::LUA_TFUNCTION {
            ffi::lua_pushvalue(state, members);
            protect_lua_closure(state, 2, 1, |state| {
                ffi::lua_pushcclosure(state, meta_index_impl, 2);
            })?;
        } else {
            rlua_panic!("improper __index type {}", index_type);
        }

        protect_lua_closure(state, 3, 1, |state| {
            ffi::lua_rawset(state, -3);
        })?;
    }

    push_string(state, "__gc")?;
    ffi::lua_pushcfunction(state, userdata_destructor::<T>);
    protect_lua_closure(state, 3, 1, |state| {
        ffi::lua_rawset(state, -3);
    })?;

    push_string(state, "__metatable")?;
    ffi::lua_pushboolean(state, 0);
    protect_lua_closure(state, 3, 1, |state| {
        ffi::lua_rawset(state, -3);
    })?;

    ffi::lua_pop(state, 1);

    Ok(())
}

pub unsafe extern "C" fn userdata_destructor<T>(state: *mut ffi::lua_State) -> c_int {
    callback_error(state, |_| {
        check_stack(state, 1)?;
        take_userdata::<T>(state);
        Ok(0)
    })
}

// In the context of a lua callback, this will call the given function and if the given function
// returns an error, *or if the given function panics*, this will result in a call to lua_error (a
// longjmp).  The error or panic is wrapped in such a way that when calling pop_error back on
// the rust side, it will resume the panic.
//
// This function assumes the structure of the stack at the beginning of a callback, that the only
// elements on the stack are the arguments to the callback.
//
// This function uses some of the bottom of the stack for error handling, the given callback will be
// given the number of arguments available as an argument, and should return the number of returns
// as normal, but cannot assume that the arguments available start at 0.
pub unsafe fn callback_error<R, F>(state: *mut ffi::lua_State, f: F) -> R
where
    F: FnOnce(c_int) -> Result<R>,
{
    let nargs = ffi::lua_gettop(state);

    // We need one extra stack space to store preallocated memory, and at least 3 stack spaces
    // overall for handling error metatables
    let extra_stack = if nargs < 3 { 3 - nargs } else { 1 };
    ffi::luaL_checkstack(
        state,
        extra_stack,
        cstr!("not enough stack space for callback error handling"),
    );

    // We cannot shadow rust errors with Lua ones, we pre-allocate enough memory to store a wrapped
    // error or panic *before* we proceed.
    // We don't need any user values in this userdata
    let ud = ffi::lua_newuserdatauv(
        state,
        mem::size_of::<WrappedError>().max(mem::size_of::<WrappedPanic>()),
        0,
    );
    ffi::lua_rotate(state, 1, 1);

    match catch_unwind(AssertUnwindSafe(|| f(nargs))) {
        Ok(Ok(r)) => {
            ffi::lua_rotate(state, 1, -1);
            ffi::lua_pop(state, 1);
            r
        }
        Ok(Err(err)) => {
            ffi::lua_settop(state, 1);
            ptr::write(ud as *mut WrappedError, WrappedError(err));
            get_error_metatable(state);
            ffi::lua_setmetatable(state, -2);
            ffi::lua_error(state);
            panic!("lua_error");
        }
        Err(p) => {
            ffi::lua_settop(state, 1);
            ptr::write(ud as *mut WrappedPanic, WrappedPanic(Some(p)));
            get_panic_metatable(state);
            ffi::lua_setmetatable(state, -2);
            ffi::lua_error(state);
            panic!("lua_error");
        }
    }
}

// Takes an error at the top of the stack, and if it is a WrappedError, converts it to an
// Error::CallbackError with a traceback, if it is some lua type, prints the error along with a
// traceback, and if it is a WrappedPanic, does not modify it.  This function does its best to avoid
// triggering another error and shadowing previous rust errors, but it may trigger Lua errors that
// shadow rust errors under certain memory conditions.  This function ensures that such behavior
// will *never* occur with a rust panic, however.
pub unsafe extern "C" fn error_traceback(state: *mut ffi::lua_State) -> c_int {
    // I believe luaL_traceback requires this much free stack to not error.
    const LUA_TRACEBACK_STACK: c_int = 11;

    if ffi::lua_checkstack(state, 2) == 0 {
        // If we don't have enough stack space to even check the error type, do nothing so we don't
        // risk shadowing a rust panic.
    } else if let Some(error) = get_wrapped_error(state, -1).as_ref() {
        // lua_newuserdatauv and luaL_traceback may error, but nothing that implements Drop should be
        // on the rust stack at this time.
        // We don't need any user values in this userdata
        let ud =
            ffi::lua_newuserdatauv(state, mem::size_of::<WrappedError>(), 0) as *mut WrappedError;
        let traceback = if ffi::lua_checkstack(state, LUA_TRACEBACK_STACK) != 0 {
            ffi::luaL_traceback(state, state, ptr::null(), 0);

            let traceback = to_string(state, -1).into_owned();
            ffi::lua_pop(state, 1);
            traceback
        } else {
            "<not enough stack space for traceback>".to_owned()
        };

        let error = error.clone();
        ffi::lua_remove(state, -2);

        ptr::write(
            ud,
            WrappedError(Error::CallbackError {
                traceback,
                cause: Arc::new(error),
            }),
        );
        get_error_metatable(state);
        ffi::lua_setmetatable(state, -2);
    } else if !is_wrapped_panic(state, -1) {
        if ffi::lua_checkstack(state, LUA_TRACEBACK_STACK) != 0 {
            let s = ffi::luaL_tolstring(state, -1, ptr::null_mut());
            ffi::luaL_traceback(state, state, s, 0);
            ffi::lua_remove(state, -2);
        }
    }
    1
}

// A variant of pcall that does not allow lua to catch panic errors from callback_error
pub unsafe extern "C" fn safe_pcall(state: *mut ffi::lua_State) -> c_int {
    ffi::luaL_checkstack(state, 2, ptr::null());

    let top = ffi::lua_gettop(state);
    if top == 0 {
        ffi::lua_pushstring(state, cstr!("not enough arguments to pcall"));
        ffi::lua_error(state);
        panic!("lua_error");
    } else if ffi::lua_pcall(state, top - 1, ffi::LUA_MULTRET, 0) != ffi::LUA_OK {
        if is_wrapped_panic(state, -1) {
            ffi::lua_error(state);
            panic!("lua_error");
        }
        ffi::lua_pushboolean(state, 0);
        ffi::lua_insert(state, -2);
        2
    } else {
        ffi::lua_pushboolean(state, 1);
        ffi::lua_insert(state, 1);
        ffi::lua_gettop(state)
    }
}

// A variant of xpcall that does not allow lua to catch panic errors from callback_error
pub unsafe extern "C" fn safe_xpcall(state: *mut ffi::lua_State) -> c_int {
    unsafe extern "C" fn xpcall_msgh(state: *mut ffi::lua_State) -> c_int {
        ffi::luaL_checkstack(state, 2, ptr::null());

        if is_wrapped_panic(state, -1) {
            1
        } else {
            ffi::lua_pushvalue(state, ffi::lua_upvalueindex(1));
            ffi::lua_insert(state, 1);
            ffi::lua_call(state, ffi::lua_gettop(state) - 1, ffi::LUA_MULTRET);
            ffi::lua_gettop(state)
        }
    }

    ffi::luaL_checkstack(state, 2, ptr::null());

    let top = ffi::lua_gettop(state);
    if top < 2 {
        ffi::lua_pushstring(state, cstr!("not enough arguments to xpcall"));
        ffi::lua_error(state);
        panic!("lua_error");
    }

    ffi::lua_pushvalue(state, 2);
    ffi::lua_pushcclosure(state, xpcall_msgh, 1);
    ffi::lua_copy(state, 1, 2);
    ffi::lua_replace(state, 1);

    let res = ffi::lua_pcall(state, ffi::lua_gettop(state) - 2, ffi::LUA_MULTRET, 1);
    if res != ffi::LUA_OK {
        if is_wrapped_panic(state, -1) {
            ffi::lua_error(state);
            panic!("lua_error");
        }
        ffi::lua_pushboolean(state, 0);
        ffi::lua_insert(state, -2);
        2
    } else {
        ffi::lua_pushboolean(state, 1);
        ffi::lua_insert(state, 2);
        ffi::lua_gettop(state) - 1
    }
}

// Pushes a WrappedError to the top of the stack.  Uses two stack spaces and does not call
// lua_checkstack.
pub unsafe fn push_wrapped_error(state: *mut ffi::lua_State, err: Error) -> Result<()> {
    // We don't need any user values in this userdata
    // TODO: temp
    let ud = protect_lua_closure(state, 0, 1, move |state| {
        ffi::lua_newuserdatauv(state, mem::size_of::<WrappedError>(), 0) as *mut WrappedError
    })?;
    ptr::write(ud, WrappedError(err));
    get_error_metatable(state);
    ffi::lua_setmetatable(state, -2);
    Ok(())
}

// Checks if the value at the given index is a WrappedError, and if it is returns a pointer to it,
// otherwise returns null.  Uses 2 stack spaces and does not call lua_checkstack.
pub unsafe fn get_wrapped_error(state: *mut ffi::lua_State, index: c_int) -> *const Error {
    let userdata = ffi::lua_touserdata(state, index);
    if userdata.is_null() {
        return ptr::null();
    }

    if ffi::lua_getmetatable(state, index) == 0 {
        return ptr::null();
    }

    get_error_metatable(state);
    let res = ffi::lua_rawequal(state, -1, -2) != 0;
    ffi::lua_pop(state, 2);

    if res {
        &(*get_userdata::<WrappedError>(state, -1)).0
    } else {
        ptr::null()
    }
}

// Initialize the error, panic, and destructed userdata metatables.
pub unsafe fn init_error_registry(state: *mut ffi::lua_State) {
    assert_stack(state, 8);

    // Create error metatable

    unsafe extern "C" fn error_tostring(state: *mut ffi::lua_State) -> c_int {
        let err_buf = callback_error(state, |_| {
            check_stack(state, 3)?;
            if let Some(error) = get_wrapped_error(state, -1).as_ref() {
                ffi::lua_pushlightuserdata(
                    state,
                    &ERROR_PRINT_BUFFER_KEY as *const u8 as *mut c_void,
                );
                ffi::lua_rawget(state, ffi::LUA_REGISTRYINDEX);
                let err_buf = ffi::lua_touserdata(state, -1) as *mut String;
                ffi::lua_pop(state, 2);

                (*err_buf).clear();
                // Depending on how the API is used and what error types scripts are given, it may
                // be possible to make this consume arbitrary amounts of memory (for example, some
                // kind of recursive error structure?)
                let _ = write!(&mut (*err_buf), "{}", error);
                Ok(err_buf)
            } else {
                // I'm not sure whether this is possible to trigger without bugs in rlua?
                Err(Error::UserDataTypeMismatch)
            }
        });

        ffi::lua_pushlstring(
            state,
            (*err_buf).as_ptr() as *const c_char,
            (*err_buf).len(),
        );
        (*err_buf).clear();
        1
    }

    ffi::lua_pushlightuserdata(
        state,
        &ERROR_METATABLE_REGISTRY_KEY as *const u8 as *mut c_void,
    );
    ffi::lua_newtable(state);

    ffi::lua_pushstring(state, cstr!("__gc"));
    ffi::lua_pushcfunction(state, userdata_destructor::<WrappedError>);
    ffi::lua_rawset(state, -3);

    ffi::lua_pushstring(state, cstr!("__tostring"));
    ffi::lua_pushcfunction(state, error_tostring);
    ffi::lua_rawset(state, -3);

    ffi::lua_pushstring(state, cstr!("__metatable"));
    ffi::lua_pushboolean(state, 0);
    ffi::lua_rawset(state, -3);

    ffi::lua_rawset(state, ffi::LUA_REGISTRYINDEX);

    // Create panic metatable

    ffi::lua_pushlightuserdata(
        state,
        &PANIC_METATABLE_REGISTRY_KEY as *const u8 as *mut c_void,
    );
    ffi::lua_newtable(state);

    ffi::lua_pushstring(state, cstr!("__gc"));
    ffi::lua_pushcfunction(state, userdata_destructor::<WrappedPanic>);
    ffi::lua_rawset(state, -3);

    ffi::lua_pushstring(state, cstr!("__metatable"));
    ffi::lua_pushboolean(state, 0);
    ffi::lua_rawset(state, -3);

    ffi::lua_rawset(state, ffi::LUA_REGISTRYINDEX);

    // Create destructed userdata metatable

    unsafe extern "C" fn destructed_error(state: *mut ffi::lua_State) -> c_int {
        ffi::luaL_checkstack(state, 2, ptr::null());
        // We don't need any user values in this userdata
        let ud =
            ffi::lua_newuserdatauv(state, mem::size_of::<WrappedError>(), 0) as *mut WrappedError;

        ptr::write(ud, WrappedError(Error::CallbackDestructed));
        get_error_metatable(state);
        ffi::lua_setmetatable(state, -2);
        ffi::lua_error(state);
        panic!("lua_error");
    }

    ffi::lua_pushlightuserdata(
        state,
        &DESTRUCTED_USERDATA_METATABLE as *const u8 as *mut c_void,
    );
    ffi::lua_newtable(state);

    for &method in &[
        cstr!("__add"),
        cstr!("__sub"),
        cstr!("__mul"),
        cstr!("__div"),
        cstr!("__mod"),
        cstr!("__pow"),
        cstr!("__unm"),
        cstr!("__idiv"),
        cstr!("__band"),
        cstr!("__bor"),
        cstr!("__bxor"),
        cstr!("__bnot"),
        cstr!("__shl"),
        cstr!("__shr"),
        cstr!("__concat"),
        cstr!("__len"),
        cstr!("__eq"),
        cstr!("__lt"),
        cstr!("__le"),
        cstr!("__index"),
        cstr!("__newindex"),
        cstr!("__call"),
        cstr!("__tostring"),
        cstr!("__pairs"),
        cstr!("__ipairs"),
    ] {
        ffi::lua_pushstring(state, method);
        ffi::lua_pushcfunction(state, destructed_error);
        ffi::lua_rawset(state, -3);
    }

    ffi::lua_rawset(state, ffi::LUA_REGISTRYINDEX);

    // Create error print buffer

    ffi::lua_pushlightuserdata(state, &ERROR_PRINT_BUFFER_KEY as *const u8 as *mut c_void);

    // We don't need any user values in this userdata
    let ud = ffi::lua_newuserdatauv(state, mem::size_of::<String>(), 0) as *mut String;
    ptr::write(ud, String::new());

    ffi::lua_newtable(state);
    ffi::lua_pushstring(state, cstr!("__gc"));
    ffi::lua_pushcfunction(state, userdata_destructor::<String>);
    ffi::lua_rawset(state, -3);
    ffi::lua_setmetatable(state, -2);

    ffi::lua_rawset(state, ffi::LUA_REGISTRYINDEX);
}

struct WrappedError(pub Error);
struct WrappedPanic(pub Option<Box<dyn Any + Send>>);

// Converts the given lua value to a string in a reasonable format without causing a Lua error or
// panicking.
unsafe fn to_string<'a>(state: *mut ffi::lua_State, index: c_int) -> Cow<'a, str> {
    match ffi::lua_type(state, index) {
        ffi::LUA_TNONE => "<none>".into(),
        ffi::LUA_TNIL => "<nil>".into(),
        ffi::LUA_TBOOLEAN => (ffi::lua_toboolean(state, index) != 1).to_string().into(),
        ffi::LUA_TLIGHTUSERDATA => {
            format!("<lightuserdata {:?}>", ffi::lua_topointer(state, index)).into()
        }
        ffi::LUA_TNUMBER => {
            let mut isint = 0;
            let i = ffi::lua_tointegerx(state, -1, &mut isint);
            if isint == 0 {
                ffi::lua_tonumber(state, index).to_string().into()
            } else {
                i.to_string().into()
            }
        }
        ffi::LUA_TSTRING => {
            let mut size = 0;
            let data = ffi::lua_tolstring(state, index, &mut size);
            String::from_utf8_lossy(slice::from_raw_parts(data as *const u8, size))
        }
        ffi::LUA_TTABLE => format!("<table {:?}>", ffi::lua_topointer(state, index)).into(),
        ffi::LUA_TFUNCTION => format!("<function {:?}>", ffi::lua_topointer(state, index)).into(),
        ffi::LUA_TUSERDATA => format!("<userdata {:?}>", ffi::lua_topointer(state, index)).into(),
        ffi::LUA_TTHREAD => format!("<thread {:?}>", ffi::lua_topointer(state, index)).into(),
        _ => "<unknown>".into(),
    }
}

// Checks if the value at the given index is a WrappedPanic.  Uses 2 stack spaces and does not call
// lua_checkstack.
unsafe fn is_wrapped_panic(state: *mut ffi::lua_State, index: c_int) -> bool {
    let userdata = ffi::lua_touserdata(state, index);
    if userdata.is_null() {
        return false;
    }

    if ffi::lua_getmetatable(state, index) == 0 {
        return false;
    }

    get_panic_metatable(state);
    let res = ffi::lua_rawequal(state, -1, -2) != 0;
    ffi::lua_pop(state, 2);
    res
}

unsafe fn get_error_metatable(state: *mut ffi::lua_State) {
    ffi::lua_pushlightuserdata(
        state,
        &ERROR_METATABLE_REGISTRY_KEY as *const u8 as *mut c_void,
    );
    ffi::lua_rawget(state, ffi::LUA_REGISTRYINDEX);
}

unsafe fn get_panic_metatable(state: *mut ffi::lua_State) {
    ffi::lua_pushlightuserdata(
        state,
        &PANIC_METATABLE_REGISTRY_KEY as *const u8 as *mut c_void,
    );
    ffi::lua_rawget(state, ffi::LUA_REGISTRYINDEX);
}

unsafe fn get_destructed_userdata_metatable(state: *mut ffi::lua_State) {
    ffi::lua_pushlightuserdata(
        state,
        &DESTRUCTED_USERDATA_METATABLE as *const u8 as *mut c_void,
    );
    ffi::lua_rawget(state, ffi::LUA_REGISTRYINDEX);
}

static ERROR_METATABLE_REGISTRY_KEY: u8 = 0;
static PANIC_METATABLE_REGISTRY_KEY: u8 = 0;
static DESTRUCTED_USERDATA_METATABLE: u8 = 0;
static ERROR_PRINT_BUFFER_KEY: u8 = 0;
