# Mutation patterns

cargo mutants generates mutants by inspecting the existing
source code and applying a set of rules to generate new code
that is likely to compile but have different behavior.

Mutants each have a "genre", each of which is described below.

## Functions that are excluded from mutation

Some functions are automatically excluded from mutation:

- Functions marked with `#[cfg(test)]` or in files marked with `#![cfg(test)]`
- Test functions: functions with attributes whose path ends with `test`, including `#[test]`, `#[tokio::test]`, `#[sqlx::test]`, and similar testing framework attributes
- Functions marked with `#[mutants::skip]`
- `unsafe` functions

You can also explicitly [skip functions](skip.md) or [filter which functions are mutated](filter_mutants.md).

## Replace function body with value

The `FnValue` genre of mutants replaces a function's body with a value that is guessed to be of the right type.

This checks that the tests:

1. Observe any side effects of the original function.
2. Distinguish return values.

More mutation genres and patterns will be added in future releases.

| Return type       | Mutation pattern |
| ----------------- | ---------------- |
| `()`              | `()` (return unit, with no side effects) |
| signed integers   | `0, 1, -1`    |
| unsigned integers | `0, 1`      |
| floats            | `0.0, 1.0, -1.0`                                           |
| `NonZeroI*`       | `1.try_into().unwrap(), (-1).try_into().unwrap()`            |
| `NonZeroU*`       | `1.try_into().unwrap()`                                    |
| `NonZero<T>`      | `1.try_into().unwrap(), (-1).try_into().unwrap()`: negative values are omitted if `T` seems to be unsigned |
| `bool`            | `true`, `false` |
| `String`          | `String::new()`, `"xyzzy".into()` |
| `&'_ str` .       | `""`, `"xyzzy"` |
| `char`            | `'\0'`, `'x'` |
| `PathBuf`, `Utf8PathBuf` | `PathBuf::new()`, `PathBuf::from("xyzzy")` |
| `OsString`        | `OsString::new()`, `OsString::from("xyzzy")` |
| `Duration`        | `Duration::ZERO`, `Duration::from_secs(1)` |
| `cmp::Ordering`   | `Ordering::Less`, `Ordering::Equal`, `Ordering::Greater` |
| enum declared in the same file | each unit variant, e.g. `Colour::Red` |
| `Range<T>`        | `Default::default()` |
| `&T`              | `Box::leak(Box::new(...))` |
| `&mut T`          | `Box::leak(Box::new(...))` |
| `&[T]`            | `Vec::leak(...)` |
| `&mut [T]`            | `Vec::leak(...)` |
| `Result<T, E>`    | `Ok(...)`, `Err(...)` when `E` is a type whose values can be constructed directly (such as `String`), [and an error if configured](error-values.md) |
| `Option<T>`       | `Some(...)`, `None` |
| `Box<T>`          | `Box::new(...)`                                            |
| `Vec<T>`          | `vec![]`, `vec![...]`                                      |
| `Arc<T>`          | `Arc::new(...)`                                            |
| `Rc<T>`           | `Rc::new(...)`                                             |
| `BinaryHeap`, `BTreeSet`, `HashSet`, `LinkedList`, `VecDeque` | empty and one-element collections |
| `BTreeMap`, `HashMap` | empty map and the product of all key and value replacements |
| `Cow<'_, T>`      | `Cow::Borrowed(t)`, `Cow::Owned(t.to_owned())`             |
| `[T; L]`          | `[r; L]` for all replacements of T                         |
| `&[T]`, `&mut [T]`| Leaked empty and one-element vecs                          |
| `&T`              | `&...` (all replacements for T)                            |
| `HttpResponse`    | `HttpResponse::Ok().finish`                                |
| `(A, B, ...)`     | `(a, b, ...)` for the product of all replacements of A, B, ... |
| `impl Iterator`   | Empty and one-element iterators of the inner type           |
| (any other)       | `Default::default()`                                       |
| `dyn Trait`, `Pin`, `NonNull`, `Instant`, `SystemTime` | no mutants: no value of these types can be constructed here |

`...` in the mutation patterns indicates that the type is recursively mutated.
 For example, `Result<bool>` can generate `Ok(true)` and `Ok(false)`.
The recursion can nest for types like `Result<Option<String>>`.

Some of these values may not be valid for all types: for example, returning
`Default::default()` will work for many types, but not all. In this case the
mutant is said to be "unviable": by default these are counted but not printed,
although they can be shown with `--unviable`.

## Binary operators

Binary operators are replaced with other binary operators in expressions
like `a == 0`.

| Operator | Replacements       |
| -------- | ------------------ |
| `==`     | `!=`               |
| `!=`     | `==`               |
| `&&`     | `\|\|`             |
| `\|\|`   | `&&`,              |
| `<`      | `==`, `>`          |
| `>`      | `==`, `<`          |
| `<=`     | `>`, `<`           |
| `>=`     | `<`, `>`           |
| `+`      | `-`, `*`           |
| `-`      | `+`, `/`           |
| `*`      | `+`, `/`           |
| `/`      | `%`, `*`           |
| `%`      | `/`, `+`           |
| `<<`     | `>>`               |
| `>>`     | `<<`               |
| `&`      | `\|`,`^`           |
| `\|`     | `&`, `^`           |
| `^`      | `&`, `\|`          |
| `&=`     | `\|=`              |
| `\|=`    | `&=`               |
| `^=`     | `\|=`, `&=`        |
| `+=`, `-=`, `*=`, `/=`, `%=`, `<<=`, `>>=` | assignment corresponding to the operator above |

Equality operators are not currently replaced with comparisons like `<` or `<=`
because they are
too prone to generate false positives, for example when unsigned integers are compared to 0.

The bitwise assignment operators `&=` and `|=` are not mutated to `^=` because in code that accumulates bits (e.g., `bitmap |= new_bits`), `|=` and `^=` produce the same result when starting from zero, making such mutations uninformative.

Mutating `x <= 0` to `x < 0` on an unsigned type triggers the `unused_comparisons`
lint, which is an error in trees that deny warnings, so the mutant is counted as
unviable. See [lints](lints.md) for `--cap-lints`.

## Unary operators

Unary operators are deleted in expressions like `-a` and `!a`.
They are not currently replaced with other unary operators because they are too prone to
generate unviable cases (e.g. `!1.0`, `-false`).

## If conditions

The condition of an `if` expression is replaced with `true` and `false`, checking
that the tests exercise both sides of the branch.

`if let` conditions are not mutated because they are not `bool`. A condition that
is already a literal is covered by [bool literals](#bool-literals) instead.

The `false` mutant is not generated when the `if` body contains `break` or
`continue`, because that condition is how an enclosing loop ends, and removing
the only exit hangs the test rather than failing it.

## While conditions

The condition of a `while` expression is replaced with `false`, so the loop body
never runs. It is not replaced with `true`, which would loop forever.

## Bool literals

`true` and `false` literals are replaced with each other, in expressions like
`let debug = false;`.

Literals in patterns are not mutated, because replacing `true` with `false` in
`match b { true => .., false => .. }` makes the match non-exhaustive. Literals
inside attributes are never mutated. A literal that is the entire function body
or an entire match arm guard is left to the `FnValue` and `MatchArmGuard` genres,
which generate the same mutant.

## Statement deletion

Statements whose value is discarded are deleted, in code like
`v.push(1);` or `self.count = n;`. This checks that the tests observe the effect
of each statement.

Only calls, method calls, and plain assignments are deleted. Compound
assignments such as `n -= 1` are not, since they are often the step of a loop and
deleting them hangs the test. A statement is also kept when it is the only
constraint on the type of a `let` binding without a type annotation, as in
`let mut v = Vec::new(); v.push(1u32);`, where deleting it would leave the type
uninferable, and when it calls a function excluded by
[`--skip-calls`](skip_calls.md).

## Return values

The value of an explicit `return` is replaced with values guessed from the
function's declared return type, using the same patterns as
[`FnValue`](#replace-function-body-with-value). This checks that the tests
distinguish early returns from the value the function otherwise produces.

`return` inside a closure or `async` block is not mutated, because its type is
usually inferred and so unknown here. A function whose whole body is
`return expr;` is left to `FnValue`, which generates the same mutant.

## Match arms

Entire match arms are deleted in match expressions when a wildcard pattern is present in one of the arms.
Match expressions without a wildcard pattern would be too prone to unviable mutations of this kind.

## Match arm guards

Match arm guard expressions are replaced with `true` and `false`.

## Struct literal fields

Individual fields are deleted from struct literals that have a base (default) expression,
such as `..Default::default()` or `..base_value`.

For example, in this code:

```rust
let cat = Cat {
    name: "Felix",
    coat: Coat::Tuxedo,
    ..Default::default()
};
```

cargo-mutants will generate two mutants: one deleting the `name` field and one deleting
the `coat` field. This checks that tests verify that each field is set correctly and not
just relying on the default values.

Struct literals without a base expression are not mutated in this way, because deleting
a required field would make the code fail to compile.
