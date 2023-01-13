# Error docs style spec

This document has been extracted from [RFC 1567] and specifies how error code explanations are expected to look. 

## Spec

### Error description

Provide a more detailed error message. For example:

```rust
extern crate a;
extern crate b as a;
```

We get the `E0259` error code which says "an extern crate named `a` has already been imported in this module" and the error explanation says: "The name chosen for an external crate conflicts with another external crate that has been imported into the current module.".

### Minimal example

Provide an erroneous code example which directly follows `Error description`. The erroneous example will be helpful for the `How to fix the problem`. Making it as simple as possible is really important in order to help readers to understand what the error is about. A comment should be added with the error on the same line where the errors occur. Example:

```rust
type X = u32<i32>; // error: type parameters are not allowed on this type
```

If the error comments is too long to fit 80 columns, split it up like this, so the next line start at the same column of the previous line:

```rust
type X = u32<'static>; // error: lifetime parameters are not allowed on
                       //        this type
```

And if the sample code is too long to write an effective comment, place your comment on the line before the sample code:

```rust
// error: lifetime parameters are not allowed on this type
fn super_long_function_name_and_thats_problematic() {}
```

Of course, it the comment is too long, the split rules still applies.

### Error explanation

Provide a full explanation about "__why__ you get the error" and some leads on __how__ to fix it. If needed, use additional code snippets to improve your explanations.

### How to fix the problem

This part will show how to fix the error that we saw previously in the `Minimal example`, with comments explaining how it was fixed.

## Example

The full template looks like this:

```
[Error description]

Example of erroneous code:

\```compile_fail
[Minimal example]
\```

[Error explanation]

\```
[How to fix the problem]
\```

[Optional Additional information]
```

Now let's take a full example:

```
Patterns used to bind names must be irrefutable, that is, they must guarantee
that a name will be extracted in all cases.

Erroneous code example:

\```compile_fail,E0005
let x = Some(1);

// error: refutable pattern in local binding: `None` not covered
let Some(y) = x;
\```

If you encounter this error you probably need to use a `match` to deal with the
possibility of failure.

\```
let x = Some(1);

match x {
    Some(y) => {
        // do something
    },
    None => {}
}
\```
```