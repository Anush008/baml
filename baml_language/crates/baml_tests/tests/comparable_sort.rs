//! Spike tests for the `Comparable` / `Sortable` array-sort design
//! (thoughts/sam-projects/array-sort/01b-option-5-tdd-plan.md, Phase 1).
//!
//! These tests pin the three pieces of type-system machinery the design
//! depends on, using throwaway interfaces so failures point at the compiler
//! rather than at the stdlib:
//!
//!   1a. An associated-type *binding whose value is a projection off the
//!       impl's type variable* (`type WE = (T as HasErr).E`) inside a blanket
//!       impl for `T[]`, with the projection used in `throws` position and
//!       normalized at concrete call sites (`never` ⇒ no handling required).
//!   1b. Method resolution of blanket-impl interface methods on *array
//!       receivers* via direct call syntax (`xs.method()`), the call-site
//!       error for non-qualifying element types, and the BEP-044 rule that a
//!       class method shadows a same-named interface method.
//!   1c. A two-`Self` method (`compare(self, other: Self)`) called through a
//!       bounded type variable.
//!
//! Later phases (the real `Comparable`/`Sortable` stdlib work) build on these;
//! see `baml_src/ns_arrays/arrays.baml` for the runtime characterization net.

use std::collections::HashSet;

use baml_compiler_diagnostics::Severity;
use baml_project::{collect_diagnostics, testing::setup_test_db};
use baml_tests::baml_test;
use bex_engine::BexExternalValue;

fn collect_compile_errors(source: &str) -> Vec<String> {
    let db = setup_test_db(source);
    let project = db.get_project().expect("project must be set");
    let all_files = db.get_source_files();
    let user_file_ids: HashSet<_> = all_files.iter().map(|f| f.file_id(&db)).collect();

    collect_diagnostics(&db, project, &all_files)
        .into_iter()
        .filter(|d| matches!(d.severity, Severity::Error))
        .filter(|d| {
            d.primary_span()
                .map(|span| user_file_ids.contains(&span.file_id))
                .unwrap_or(false)
        })
        .map(|d| format!("[{}] {}", d.code(), d.message))
        .collect()
}

#[track_caller]
fn assert_zero_compile_errors(source: &str) {
    let errors = collect_compile_errors(source);
    assert!(
        errors.is_empty(),
        "expected zero compile errors, got:\n  {}",
        errors.join("\n  ")
    );
}

#[track_caller]
fn assert_compile_error_contains(source: &str, needle: &str) {
    let errors = collect_compile_errors(source);
    assert!(
        errors.iter().any(|e| e.contains(needle)),
        "expected a compile error containing {needle:?}; got:\n  {}",
        errors.join("\n  ")
    );
}

// ── 1a: projection-valued associated-type binding in a blanket impl ─────────
//
// The throwaway scaffolding mirrors the eventual `Comparable`/`Sortable`
// shape: `HasErr` plays `Comparable` (associated error `E`), `Wrap` plays
// `Sortable` (associated error `WE` bound per-impl to the *projection*
// `(T as HasErr).E`).

const SPIKE_1A_SCAFFOLD: &str = r#"
    interface HasErr {
        type E
        function f(self) -> int throws E
    }

    interface Wrap {
        type WE
        function g(self) -> int throws WE
    }

    implements<T extends HasErr> Wrap for T[] {
        type WE = T.E

        // NB: spelling matters throughout this impl. In throws position
        // neither `WE` nor `Self.WE` resolves (E0002), and the checker does
        // not unify the qualified projection `(T as HasErr).E` with the
        // body's inferred shorthand `T.E` (E0096/E0120) — so the binding,
        // the signature, and the body must all agree on the `T.E` spelling.
        function g(self) -> int throws T.E {
            if (self.length() == 0) { return 0 }
            return self[0].f()
        }
    }
"#;

#[test]
fn spike_1a_projection_valued_assoc_binding_in_blanket_impl_compiles() {
    assert_zero_compile_errors(SPIKE_1A_SCAFFOLD);
}

#[tokio::test]
async fn spike_1a_projection_normalizes_to_never_at_concrete_callsite() {
    // `Safe.E = never`, so `(Safe as HasErr).E = never` and `xs.g()` requires
    // no error handling: `main` declares no `throws` and uses no `catch`.
    let output = baml_test!(&format!(
        r#"
        {SPIKE_1A_SCAFFOLD}

        class Safe {{
            value: int
            implements HasErr {{
                type E = never
                function f(self) -> int throws never {{ return self.value }}
            }}
        }}

        function main() -> int throws never {{
            let xs: Safe[] = [Safe {{ value: 7 }}]
            return xs.g()
        }}
        "#
    ));
    assert_eq!(output.result.unwrap(), BexExternalValue::Int(7));
}

#[test]
fn spike_1a_projection_with_concrete_error_requires_handling() {
    // `Risky.E = Kaboom`, so `xs.g()` throws `Kaboom` at the call site; a
    // caller that neither catches nor declares it must not compile.
    assert_compile_error_contains(
        &format!(
            r#"
            {SPIKE_1A_SCAFFOLD}

            class Kaboom {{
                message: string
            }}

            class Risky {{
                implements HasErr {{
                    type E = Kaboom
                    function f(self) -> int throws Kaboom {{
                        throw Kaboom {{ message: "kaboom" }}
                    }}
                }}
            }}

            // An undeclared `throws` is *inferred*, so the unhandled-error
            // check only fires against an explicit declaration.
            function main() -> int throws never {{
                let xs: Risky[] = [Risky {{}}]
                return xs.g()
            }}
            "#
        ),
        "Kaboom",
    );
}

#[tokio::test]
async fn spike_1a_concrete_error_is_catchable_at_callsite() {
    let output = baml_test!(&format!(
        r#"
        {SPIKE_1A_SCAFFOLD}

        class Kaboom {{
            message: string
        }}

        class Risky {{
            implements HasErr {{
                type E = Kaboom
                function f(self) -> int throws Kaboom {{
                    throw Kaboom {{ message: "kaboom" }}
                }}
            }}
        }}

        function main() -> string {{
            let xs: Risky[] = [Risky {{}}]
            return {{
                let v = xs.g();
                "no throw"
            }} catch (e) {{
                Kaboom => "caught:" + e.message
            }}
        }}
        "#
    ));
    assert_eq!(
        output.result.unwrap(),
        BexExternalValue::String("caught:kaboom".into())
    );
}

#[tokio::test]
async fn spike_1a_symbolic_projection_propagates_through_generic_code() {
    // Inside generic code over `U extends HasErr`, `us.g()` stays symbolic:
    // the wrapper declares `throws (U as HasErr).E` and re-throws. At the
    // concrete call site the projection resolves to `Kaboom` and is caught.
    let output = baml_test!(&format!(
        r#"
        {SPIKE_1A_SCAFFOLD}

        class Kaboom {{
            message: string
        }}

        class Risky {{
            implements HasErr {{
                type E = Kaboom
                function f(self) -> int throws Kaboom {{
                    throw Kaboom {{ message: "kaboom" }}
                }}
            }}
        }}

        function call_g<U extends HasErr>(us: U[]) -> int throws U.E {{
            return us.g()
        }}

        function main() -> string {{
            let xs: Risky[] = [Risky {{}}]
            return {{
                let v = call_g(xs);
                "no throw"
            }} catch (e) {{
                Kaboom => "caught:" + e.message
            }}
        }}
        "#
    ));
    assert_eq!(
        output.result.unwrap(),
        BexExternalValue::String("caught:kaboom".into())
    );
}

#[test]
fn spike_1a_symbolic_projection_in_generic_signature_compiles() {
    assert_zero_compile_errors(&format!(
        r#"
        {SPIKE_1A_SCAFFOLD}

        function call_g<U extends HasErr>(us: U[]) -> int throws U.E {{
            return us.g()
        }}
        "#
    ));
}

#[test]
fn spike_1a_diagnostics_path_survives_non_qualifying_projection() {
    // BEP-057 flags TIR projection-on-type-variable normalization as the
    // historically crash-prone layer. Feed the diagnostics pipeline a call
    // site whose element type does NOT satisfy the bound: we must get
    // ordinary diagnostics back (any number), never a panic.
    let errors = collect_compile_errors(&format!(
        r#"
        {SPIKE_1A_SCAFFOLD}

        class NoImpl {{
            value: int
        }}

        function main() -> int {{
            let xs: NoImpl[] = [NoImpl {{ value: 1 }}]
            return xs.g()
        }}
        "#
    ));
    assert!(
        !errors.is_empty(),
        "expected a call-site error for a non-qualifying element type"
    );
}

// ── 1b: blanket-impl method resolution on array receivers ───────────────────

const SPIKE_1B_SCAFFOLD: &str = r#"
    interface Named {
        name: string
    }

    interface FirstNamed {
        function first_name(self) -> string
    }

    implements<T extends Named> FirstNamed for T[] {
        function first_name(self) -> string {
            if (self.length() == 0) { return "" }
            return self[0].name
        }
    }

    class Person {
        name: string
        implements Named {}
    }
"#;

#[tokio::test]
async fn spike_1b_blanket_method_resolves_on_array_receiver() {
    // Direct call syntax on the array receiver — not via an interface-typed
    // variable. Runtime result proves dispatch found the blanket impl.
    let output = baml_test!(&format!(
        r#"
        {SPIKE_1B_SCAFFOLD}

        function main() -> string {{
            let xs: Person[] = [Person {{ name: "Ada" }}, Person {{ name: "Bob" }}]
            return xs.first_name()
        }}
        "#
    ));
    assert_eq!(
        output.result.unwrap(),
        BexExternalValue::String("Ada".into())
    );
}

#[test]
fn spike_1b_non_qualifying_element_is_compile_error_at_callsite() {
    // `Rock` does not implement `Named`, so `rocks.first_name()` must fail at
    // the call site. This is the exact error a user will see for
    // `Resume[].sort()` — Phase 6 polishes its wording; here we only pin that
    // it exists and names the offending type.
    assert_compile_error_contains(
        &format!(
            r#"
            {SPIKE_1B_SCAFFOLD}

            class Rock {{
                label: string
            }}

            function main() -> string {{
                let rocks: Rock[] = [Rock {{ label: "igneous" }}]
                return rocks.first_name()
            }}
            "#
        ),
        "Rock",
    );
}

#[tokio::test]
async fn spike_1b_blanket_interface_method_wins_over_array_class_method() {
    // FINDING (deviates from the plan's expectation): BEP-044 documents
    // class-method precedence on direct calls, but for structural array
    // receivers the blanket impl's `count` (returning -1) currently wins over
    // `Array.count()` (= length, 3). Pinned as-is. Phase 4 still deletes
    // `sort` from `class Array<T>` — under this ordering the class method
    // would be unreachable dead code, and under the BEP-044 ordering it would
    // shadow the interface; removal is correct either way.
    let output = baml_test!(
        r#"
        interface Countable {
            function count(self) -> int
        }

        implements<T> Countable for T[] {
            function count(self) -> int { return -1 }
        }

        function main() -> int {
            let xs: int[] = [1, 2, 3]
            return xs.count()
        }
        "#
    );
    assert_eq!(output.result.unwrap(), BexExternalValue::Int(-1));
}

// ── 1c: two-`Self` method called through a bounded type variable ────────────

#[tokio::test]
async fn spike_1c_two_self_method_through_bounded_typevar_runs() {
    // Both arguments are the same type variable `T`, so the `Self`s provably
    // match — exactly the shape of `a.compare(b)` inside the blanket `sort`.
    let output = baml_test!(
        r#"
        interface Cmp {
            function compare(self, other: Self) -> int
        }

        class Num {
            value: int
            implements Cmp {
                function compare(self, other: Self) -> int {
                    return self.value - other.value
                }
            }
        }

        function lt<T extends Cmp>(a: T, b: T) -> bool {
            return a.compare(b) < 0
        }

        function main() -> bool {
            return lt(Num { value: 1 }, Num { value: 2 })
        }
        "#
    );
    assert_eq!(output.result.unwrap(), BexExternalValue::Bool(true));
}

#[test]
fn spike_1c_interface_typed_values_cannot_call_two_self_method() {
    // Guard against accidental loosening of the existing `Self` rule: two
    // *interface-typed* values have unprovable `Self`s and must not compile.
    assert_compile_error_contains(
        r#"
        interface Cmp {
            function compare(self, other: Self) -> int
        }

        function bad(a: Cmp, b: Cmp) -> bool {
            return a.compare(b) < 0
        }
        "#,
        "Self",
    );
}

// ── Phase 2 repro: user-class Comparable dispatch (fast harness) ────────────

#[test]
fn phase2_user_class_missing_cmperror_binding_is_error() {
    // `Comparable.CmpError` is undefaulted, so omitting `type CmpError` is a
    // compile error — the binding is mandatory (E0001), distinct from the
    // method's `throws` (E0120).
    assert_compile_error_contains(
        r#"
        class Score {
            points: int
            implements baml.Comparable {
                function compare(self, other: Self) -> int throws never {
                    if (self.points < other.points) { -1 }
                    else if (self.points > other.points) { 1 }
                    else { 0 }
                }
            }
        }
        "#,
        "CmpError",
    );
}

#[tokio::test]
async fn phase2_user_class_compare_direct_call_in_main() {
    let output = baml_test!(
        r#"
        class Score {
            points: int
            implements baml.Comparable {
                type CmpError = never
                function compare(self, other: Self) -> int throws never {
                    if (self.points < other.points) { -1 }
                    else if (self.points > other.points) { 1 }
                    else { 0 }
                }
            }
        }

        function main() -> int throws never {
            let high = Score { points: 9 }
            let low = Score { points: 1 }
            return high.compare(low)
        }
        "#
    );
    assert_eq!(output.result.unwrap(), BexExternalValue::Int(1));
}

#[tokio::test]
async fn phase2_user_class_compare_with_explicit_binding_in_main() {
    let output = baml_test!(
        r#"
        class Score {
            points: int
            implements baml.Comparable {
                type CmpError = never
                function compare(self, other: Self) -> int throws never {
                    if (self.points < other.points) { -1 }
                    else if (self.points > other.points) { 1 }
                    else { 0 }
                }
            }
        }

        function main() -> int throws never {
            let high = Score { points: 9 }
            let low = Score { points: 1 }
            return high.compare(low)
        }
        "#
    );
    assert_eq!(output.result.unwrap(), BexExternalValue::Int(1));
}

#[tokio::test]
async fn phase2_user_class_compare_direct_call_mir_optimized() {
    let output = baml_tests::baml_test_optimized!(
        r#"
        class Score {
            points: int
            implements baml.Comparable {
                type CmpError = never
                function compare(self, other: Self) -> int throws never {
                    if (self.points < other.points) { -1 }
                    else if (self.points > other.points) { 1 }
                    else { 0 }
                }
            }
        }

        function main() -> int throws never {
            let high = Score { points: 9 }
            let low = Score { points: 1 }
            return high.compare(low)
        }
        "#
    );
    assert_eq!(output.result.unwrap(), BexExternalValue::Int(1));
}

// ── Phase 3: `Sortable` blanket impl + projection normalization ─────────────
//
// Canonical scaffold mirrors the real stdlib: `Comparable`-style interface
// with an UNDEFAULTED associated error (`type CE`, no `= never`), a
// `Sortable`-style blanket impl binding `SE = T.CE`, and user classes that
// bind `CE` explicitly (to `never` or a concrete error).
//
// FINDING — a *defaulted* associated error breaks the fallible case: see
// `phase3_defaulted_assoc_error_over_constrains_bound` below. That is why the
// stdlib `Comparable.CmpError` is intentionally undefaulted.

const PHASE3_SCAFFOLD: &str = r#"
    interface Cmp2 {
        type CE
        function comp(self, other: Self) -> int throws CE
    }

    interface Srt2 {
        type SE
        function srt(self) -> Self throws SE
    }

    implements<T extends Cmp2> Srt2 for T[] {
        type SE = T.CE

        function srt(self) -> Self throws T.CE {
            self.sort_by((a: T, b: T) -> int throws T.CE { a.comp(b) })
        }
    }

    class Boom2 { message: string }

    class WithNever {
        v: int
        implements Cmp2 {
            type CE = never
            function comp(self, other: Self) -> int throws never { return 0 }
        }
    }

    class WithErr {
        v: int
        implements Cmp2 {
            type CE = Boom2
            function comp(self, other: Self) -> int throws Boom2 { return 0 }
        }
    }
"#;

#[test]
fn phase3_never_binding_callsite_normalizes_to_never() {
    // `WithNever.CE = never`, so `(WithNever[] as Srt2).SE` normalizes to
    // `never`: `srt()` requires no error handling.
    assert_zero_compile_errors(&format!(
        r#"
        {PHASE3_SCAFFOLD}
        function f(xs: WithNever[]) -> WithNever[] throws never {{
            xs.srt()
        }}
        "#
    ));
}

#[test]
fn phase3_error_binding_callsite_throws_it() {
    // `WithErr.CE = Boom2`, so `srt()` throws `Boom2` and the caller must
    // declare it (here) or catch it.
    assert_zero_compile_errors(&format!(
        r#"
        {PHASE3_SCAFFOLD}
        function f(xs: WithErr[]) -> WithErr[] throws Boom2 {{
            xs.srt()
        }}
        "#
    ));
}

#[test]
fn phase3_error_binding_unhandled_is_compile_error() {
    // The dual of the previous test: failing to handle `Boom2` is an error.
    assert_compile_error_contains(
        &format!(
            r#"
            {PHASE3_SCAFFOLD}
            function f(xs: WithErr[]) -> WithErr[] throws never {{
                xs.srt()
            }}
            "#
        ),
        "Boom2",
    );
}

// Issue-A regression: a projection off a *primitive* base type
// (`int.CE` via `implements Cmp2 for int`) must normalize. Before the
// `resolve_primitive_projection` fix this stayed symbolic and `int[].srt()`
// spuriously "threw `int.CE`".
#[test]
fn phase3_out_of_body_impl_on_builtin_normalizes_to_never() {
    assert_zero_compile_errors(&format!(
        r#"
        {PHASE3_SCAFFOLD}

        implements Cmp2 for int {{
            type CE = never
            function comp(self, other: Self) -> int throws never {{ return 0 }}
        }}

        function f(xs: int[]) -> int[] throws never {{
            xs.srt()
        }}
        "#
    ));
}

#[test]
fn phase3_out_of_body_impl_on_builtin_with_error_throws_it() {
    assert_zero_compile_errors(&format!(
        r#"
        {PHASE3_SCAFFOLD}

        implements Cmp2 for int {{
            type CE = Boom2
            function comp(self, other: Self) -> int throws Boom2 {{ return 0 }}
        }}

        function f(xs: int[]) -> int[] throws Boom2 {{
            xs.srt()
        }}
        "#
    ));
}

// FINDING (documents *why* the stdlib `Comparable.CmpError` is undefaulted):
// when the interface associated type has a default, a bare `T extends Cmp`
// bound is silently constrained to that default, so an implementor that
// overrides it (a fallible comparator) is rejected by the blanket impl —
// `srt` does not resolve on its array. Pinned as a known limitation.
#[test]
fn phase3_defaulted_assoc_error_over_constrains_bound() {
    let errors = collect_compile_errors(
        r#"
        interface CmpD {
            type CE = never
            function comp(self, other: Self) -> int throws CE
        }

        interface SrtD {
            type SE
            function srt(self) -> Self throws SE
        }

        implements<T extends CmpD> SrtD for T[] {
            type SE = T.CE
            function srt(self) -> Self throws T.CE {
                self.sort_by((a: T, b: T) -> int throws T.CE { a.comp(b) })
            }
        }

        class BoomD { message: string }

        class WithErrD {
            v: int
            implements CmpD {
                type CE = BoomD
                function comp(self, other: Self) -> int throws BoomD { return 0 }
            }
        }

        function f(xs: WithErrD[]) -> WithErrD[] throws BoomD {
            xs.srt()
        }
        "#,
    );
    assert!(
        errors.iter().any(|e| e.contains("srt")),
        "expected the defaulted-assoc-type bug to block `srt` resolution; got:\n  {}",
        errors.join("\n  ")
    );
}

// ── Phase 3 runtime: actually execute the blanket `sort` ───────────────────

#[tokio::test]
async fn phase3_runtime_sort_ints() {
    // join() stringifies only strings, so assert via indexing on the result.
    let output = baml_test!(
        r#"
        function main() -> int throws never {
            let xs = [3, 1, 2]
            let s = xs.sort()
            return s[0] * 100 + s[1] * 10 + s[2]
        }
        "#
    );
    assert_eq!(output.result.unwrap(), BexExternalValue::Int(123));
}

// ── Phase 3 runtime gap-mapping: generic dispatch to primitive vs class ─────

#[tokio::test]
async fn phase3_runtime_generic_compare_on_primitive() {
    let output = baml_test!(
        r#"
        function cmp<T extends baml.Comparable>(a: T, b: T) -> int throws T.CmpError {
            return a.compare(b)
        }
        function main() -> int throws never {
            let a: int = 3
            let b: int = 1
            return cmp(a, b)
        }
        "#
    );
    assert_eq!(output.result.unwrap(), BexExternalValue::Int(1));
}

#[tokio::test]
async fn phase3_runtime_generic_compare_on_user_class() {
    let output = baml_test!(
        r#"
        class Score {
            points: int
            implements baml.Comparable {
                type CmpError = never
                function compare(self, other: Self) -> int throws never {
                    return self.points.compare(other.points)
                }
            }
        }
        function cmp<T extends baml.Comparable>(a: T, b: T) -> int throws T.CmpError {
            return a.compare(b)
        }
        function main() -> int throws never {
            return cmp(Score { points: 9 }, Score { points: 1 })
        }
        "#
    );
    assert_eq!(output.result.unwrap(), BexExternalValue::Int(1));
}

#[tokio::test]
async fn phase3_runtime_sort_user_class() {
    let output = baml_test!(
        r#"
        class Score {
            points: int
            implements baml.Comparable {
                type CmpError = never
                function compare(self, other: Self) -> int throws never {
                    return self.points.compare(other.points)
                }
            }
        }
        function main() -> int throws never {
            let xs = [Score { points: 3 }, Score { points: 1 }, Score { points: 2 }]
            let sorted = xs.sort()
            return sorted[0].points
        }
        "#
    );
    assert_eq!(output.result.unwrap(), BexExternalValue::Int(1));
}

// Same body as the blanket `sort`, but as a plain generic function — isolates
// whether the failure is the blanket-impl context or generic lambda dispatch.
#[tokio::test]
async fn phase3_runtime_generic_fn_sort_by_compare_ints() {
    let output = baml_test!(
        r#"
        function gsort<T extends baml.Comparable>(xs: T[]) -> T[] throws T.CmpError {
            return xs.sort_by((a: T, b: T) -> int throws T.CmpError { a.compare(b) })
        }
        function main() -> int throws never {
            let xs = [3, 1, 2]
            let s = gsort(xs)
            return s[0] * 100 + s[1] * 10 + s[2]
        }
        "#
    );
    assert_eq!(output.result.unwrap(), BexExternalValue::Int(123));
}


#[tokio::test]
async fn phase3_runtime_sort_param_receiver_in_function() {
    // Param-receiver dispatch in a real function (not a test block). The
    // baml_src test-block versions fail only because passing a test-block
    // `let` local to a function is a known VM quirk, unrelated to sort.
    let output = baml_test!(
        r#"
        function do_sort(xs: int[]) -> int[] throws never {
            return xs.sort()
        }
        function main() -> int throws never {
            let xs = [3, 1, 2]
            let s = do_sort(xs)
            return s[0] * 100 + s[1] * 10 + s[2]
        }
        "#
    );
    assert_eq!(output.result.unwrap(), BexExternalValue::Int(123));
}

// ── Phase 5: deliberate breaking changes (now compile errors) ───────────────
//
// After the cutover, `sort()` is the `Sortable` blanket method requiring
// `T implements Comparable`. Element types that cannot implement `Comparable`
// — unions (including optionals) and user classes without an impl — no longer
// compile, where the old native `Array.sort` accepted them and threw at
// runtime.

#[test]
fn phase5_union_int_float_sort_is_compile_error() {
    let errors = collect_compile_errors(
        r#"
        function f() -> null throws never {
            let xs: (int | float)[] = [1, 2.5]
            xs.sort()
            return null
        }
        "#,
    );
    assert!(
        !errors.is_empty(),
        "expected `(int | float)[].sort()` to be a compile error after the cutover"
    );
}

#[test]
fn phase5_optional_element_sort_is_compile_error() {
    let errors = collect_compile_errors(
        r#"
        function f() -> null throws never {
            let xs: int?[] = [1, null, 2]
            xs.sort()
            return null
        }
        "#,
    );
    assert!(
        !errors.is_empty(),
        "expected `int?[].sort()` to be a compile error (optional is a union)"
    );
}

#[test]
fn phase5_union_int_string_sort_is_compile_error() {
    let errors = collect_compile_errors(
        r#"
        function f() -> null throws never {
            let xs: (int | string)[] = [2, "x", 1]
            xs.sort()
            return null
        }
        "#,
    );
    assert!(
        !errors.is_empty(),
        "expected `(int | string)[].sort()` to be a compile error"
    );
}

#[test]
fn phase5_class_without_comparable_sort_is_compile_error() {
    let errors = collect_compile_errors(
        r#"
        class Resume {
            name: string
        }
        function f() -> null throws never {
            let xs = [Resume { name: "b" }, Resume { name: "a" }]
            xs.sort()
            return null
        }
        "#,
    );
    assert!(
        !errors.is_empty(),
        "expected `Resume[].sort()` (no Comparable impl) to be a compile error"
    );
}

#[tokio::test]
async fn phase5_class_with_comparable_sort_compiles_and_runs() {
    // The dual: a class that DOES implement Comparable sorts fine.
    let output = baml_test!(
        r#"
        class Resume {
            rank: int
            implements baml.Comparable {
                type CmpError = never
                function compare(self, other: Self) -> int throws never {
                    return self.rank.compare(other.rank)
                }
            }
        }
        function main() -> int throws never {
            let xs = [Resume { rank: 3 }, Resume { rank: 1 }, Resume { rank: 2 }]
            let sorted = xs.sort()
            return sorted[0].rank
        }
        "#
    );
    assert_eq!(output.result.unwrap(), BexExternalValue::Int(1));
}


#[test]
fn phase6_sort_error_message_names_the_array_type() {
    // The `Resume[].sort()` failure (no `Comparable` impl) names the array type
    // and the missing `sort` member, with a stable `E0007` code. (A richer
    // "implement Comparable or use sort_by" hint would live in the diagnostic
    // rendering layer rather than the `TirTypeError` Display — deferred.)
    let errors = collect_compile_errors(
        r#"
        class Resume { name: string }
        function f() -> null throws never {
            let xs = [Resume { name: "b" }, Resume { name: "a" }]
            xs.sort()
            return null
        }
        "#,
    );
    assert!(
        errors
            .iter()
            .any(|e| e.starts_with("[E0007]") && e.contains("Resume[]") && e.contains("sort")),
        "expected a typed `no member sort` error on `Resume[]`; got:\n  {}",
        errors.join("\n  ")
    );
}
