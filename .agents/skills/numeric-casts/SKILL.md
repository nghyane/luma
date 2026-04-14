---
name: numeric-casts
description: Rules for numeric type conversions in Luma. Use when writing or reviewing code that casts between integer or float types, especially across different widths.
---

# Numeric Casts in Luma

## Widening casts (smaller → larger)

Always use `From`/`Into`, never `as`:

```rust
// Correct
let x: u64 = u64::from(byte);
let x: u64 = byte.into();

// Wrong — triggers clippy::cast_lossless
let x = byte as u64;
```

`clippy::cast_lossless` enforces this automatically.

## Narrowing casts (larger → smaller)

Only use `as` when the bound is provably safe and documented:

```rust
// Acceptable — bound is clear from context
let idx = offset as usize; // offset is always < usize::MAX by construction

// Preferred when bound is not obvious
let idx = usize::try_from(offset).expect("offset fits usize");
```

If overflow is possible and silent truncation is wrong, use `try_from` and handle the error explicitly.
