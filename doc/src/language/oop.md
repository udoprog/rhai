Object-Oriented Programming (OOP)
================================

{{#include ../links.md}}

Rhai does not have _objects_ per se, but it is possible to _simulate_ object-oriented programming.


Use [Object Maps] to Simulate OOP
--------------------------------

Rhai's [object maps] has [special support for OOP]({{rootUrl}}/language/object-maps-oop.md).

| Rhai concept                                          | Maps to OOP |
| ----------------------------------------------------- | :---------: |
| [Object maps]                                         |   objects   |
| [Object map] properties holding values                | properties  |
| [Object map] properties that hold [function pointers] |   methods   |

When a property of an [object map] is called like a method function, and if it happens to hold
a valid [function pointer] (perhaps defined via an [anonymous function]), then the call will be
dispatched to the actual function with `this` binding to the [object map] itself.

Examples
--------

```rust
// Define the object
let obj =
     #{
          data: 0,
          increment: |x| this.data += x,    // when called, 'this' binds to 'obj'
          update: |x| this.data = x,        // when called, 'this' binds to 'obj'
          action: || print(this.data)       // when called, 'this' binds to 'obj'
     };

// Use the object
obj.increment(1);
obj.action();                               // prints 1

obj.update(42);
obj.action();                               // prints 42
```
