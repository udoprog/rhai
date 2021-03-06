Performance Build
=================

{{#include ../../links.md}}

Use Only One Integer Type
------------------------

Some features are for performance.  For example, using [`only_i32`] or [`only_i64`] disables all other integer types (such as `u16`).

If only a single integer type is needed in scripts - most of the time this is the case - it is best to avoid registering
lots of functions related to other integer types that will never be used.  As a result, [`Engine`] creation will be faster
because fewer functions need to be loaded.


Use Only 32-Bit Numbers
----------------------

If only 32-bit integers are needed - again, most of the time this is the case - using [`only_i32`] disables also `i64`.

On 64-bit targets this may not gain much, but on some 32-bit targets this improves performance due to 64-bit arithmetic
requiring more CPU cycles to complete.


Minimize Size of [`Dynamic`]
---------------------------

Turning on [`no_float`], and [`only_i32`] makes the key [`Dynamic`] data type only 8 bytes small on 32-bit targets
while normally it can be up to 16 bytes (e.g. on x86/x64 CPU's) in order to hold an `i64` or `f64`.

Making [`Dynamic`] small helps performance due to better cache efficiency.


Use `ImmutableString`
--------------------

Internally, Rhai uses _immutable_ [strings] instead of the Rust `String` type.  This is mainly to avoid excessive
cloning when passing function arguments.

The encapsulated immutable string type is `ImmutableString`.  It is cheap to clone (just an `Rc` or `Arc` reference
count increment depending on the [`sync`] feature).

Therefore, functions taking `String` parameters should use `ImmutableString` or `&str` (which maps to `ImmutableString`)
for the best performance with Rhai.
