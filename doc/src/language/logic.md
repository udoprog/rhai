Logic Operators
==============

{{#include ../links.md}}

Comparison Operators
-------------------

Comparing most values of the same data type work out-of-the-box for all [standard types] supported by the system.

However, if using a [raw `Engine`] without loading any [packages], comparisons can only be made between a limited
set of types (see [built-in operators]).

```rust
42 == 42;               // true

42 > 42;                // false

"hello" > "foo";        // true

"42" == 42;             // false
```

Comparing two values of _different_ data types, or of unknown data types, always results in `false`,
except for '`!=`' (not equals) which results in `true`. This is in line with intuition.

```rust
42 == 42.0;             // false - i64 cannot be compared with f64

42 != 42.0;             // true - i64 cannot be compared with f64

42 > "42";              // false - i64 cannot be compared with string

42 <= "42";             // false - i64 cannot be compared with string

let ts = new_ts();      // custom type

ts == 42;               // false - types cannot be compared

ts != 42;               // true - types cannot be compared
```

Boolean operators
-----------------

| Operator          | Description                           |
| ----------------- | ------------------------------------- |
| `!`               | Boolean _Not_                         |
| `&&`              | Boolean _And_ (short-circuits)        |
| <code>\|\|</code> | Boolean _Or_ (short-circuits)         |
| `&`               | Boolean _And_ (doesn't short-circuit) |
| <code>\|</code>   | Boolean _Or_ (doesn't short-circuit)  |

Double boolean operators `&&` and `||` _short-circuit_, meaning that the second operand will not be evaluated
if the first one already proves the condition wrong.

Single boolean operators `&` and `|` always evaluate both operands.

```rust
a() || b();             // b() is not evaluated if a() is true

a() && b();             // b() is not evaluated if a() is false

a() | b();              // both a() and b() are evaluated

a() & b();              // both a() and b() are evaluated
```

Compound Assignment Operators
----------------------------

```rust
let number = 9;

number += 8;            // number = number + 8

number -= 7;            // number = number - 7

number *= 6;            // number = number * 6

number /= 5;            // number = number / 5

number %= 4;            // number = number % 4

number ~= 3;            // number = number ~ 3

number <<= 2;           // number = number << 2

number >>= 1;           // number = number >> 1

number &= 0x00ff;       // number = number & 0x00ff;

number |= 0x00ff;       // number = number | 0x00ff;

number ^= 0x00ff;       // number = number ^ 0x00ff;
```

The `+=` operator can also be used to build [strings]:

```rust
let my_str = "abc";
my_str += "ABC";
my_str += 12345;

my_str == "abcABC12345"
```
