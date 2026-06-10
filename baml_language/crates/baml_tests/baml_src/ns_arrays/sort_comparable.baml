// Tests for the `Comparable` interface (stdlib `comparable.baml`) and the
// `Sortable`-based array sort built on top of it.
//
// Phase 2 of thoughts/sam-projects/array-sort/01b-option-5-tdd-plan.md:
// builtin impls for int/float/bigint/string, the defaulted
// `type CmpError = never`, user opt-in impls, and the float-NaN decision
// (runtime `InvalidArgument`, preserving pre-Comparable semantics).

// `throws never` declarations prove at *compile time* that the builtin
// `compare` impls (and the defaulted `CmpError`) need no error handling —
// an undeclared `throws` would be inferred and hide a regression.

function comparable_cmp_ints(a: int, b: int) -> int throws never {
    a.compare(b)
}

function comparable_cmp_strings(a: string, b: string) -> int throws never {
    a.compare(b)
}

function comparable_cmp_bigints(a: bigint, b: bigint) -> int throws never {
    a.compare(b)
}

test "comparable_int_compare" {
    assert.is_true(comparable_cmp_ints(5, 3) > 0);
    assert.is_true(comparable_cmp_ints(3, 5) < 0);
    assert.is_true(comparable_cmp_ints(3, 3) == 0)
}

test "comparable_string_compare" {
    assert.is_true(comparable_cmp_strings("a", "b") < 0);
    assert.is_true(comparable_cmp_strings("b", "a") > 0);
    assert.is_true(comparable_cmp_strings("aa", "a") > 0);
    assert.is_true(comparable_cmp_strings("a", "a") == 0)
}

test "comparable_bigint_compare" {
    assert.is_true(comparable_cmp_bigints(2n, 1n) > 0);
    assert.is_true(comparable_cmp_bigints(1n, 2n) < 0);
    assert.is_true(comparable_cmp_bigints(2n, 2n) == 0)
}

// float's `CmpError` is `InvalidArgument` (the NaN decision), so its
// compare is fallible — declared, not caught, here.
function comparable_cmp_floats(a: float, b: float) -> int throws baml.errors.InvalidArgument {
    a.compare(b)
}

test "comparable_float_compare" {
    assert.is_true(comparable_cmp_floats(2.5, 1.5) > 0);
    assert.is_true(comparable_cmp_floats(1.5, 2.5) < 0);
    assert.is_true(comparable_cmp_floats(1.5, 1.5) == 0)
}

// DECISION TEST (plan Phase 2): float comparison rejects NaN at runtime with
// `InvalidArgument` — `sort()` on floats keeps today's "rejects NaN"
// behavior, while int/bigint/string improve to infallible.
test "comparable_float_nan_throws" {
    let msg = {
        let v = comparable_cmp_floats(1.0, float.nan());
        "no throw"
    } catch (e) {
        baml.errors.InvalidArgument => e.message
    };
    assert.is_true(msg.matches("NaN"))
}

// ─── user classes opting in ──────────────────────────────────────────────────
//
// KNOWN VM BUG (pre-existing, not Comparable-specific): a class instance read
// back from a *test-block local* stays boxed, and passing a boxed value to a
// two-`Self`-param interface method (`compare(self, other: Self)`) panics the
// VM with "unreachable code executed". The same calls work from ordinary
// functions. Workaround used throughout: construct instances at the call
// boundary (inline arguments), never via test-block `let`.

// Total-order user type: binds `type CmpError = never` and writes the
// matching `throws never` (an omitted `throws` is inferred, not `never`, and
// fails conformance E0120).
class ComparableScore {
    points int
    implements baml.Comparable {
        type CmpError = never
        function compare(self, other: Self) -> int throws never {
            self.points.compare(other.points)
        }
    }
}

function comparable_cmp_scores(a: ComparableScore, b: ComparableScore) -> int throws never {
    a.compare(b)
}

test "comparable_user_class_default_cmperror_is_never" {
    assert.is_true(comparable_cmp_scores(ComparableScore { points: 9 }, ComparableScore { points: 1 }) > 0);
    assert.is_true(comparable_cmp_scores(ComparableScore { points: 1 }, ComparableScore { points: 9 }) < 0);
    assert.is_true(comparable_cmp_scores(ComparableScore { points: 1 }, ComparableScore { points: 1 }) == 0)
}

// Fallible comparator: binds `type CmpError = ComparableCmpError` and throws.
class ComparableCmpError {
    message string
}

class ComparableFlaky {
    value int?
    implements baml.Comparable {
        type CmpError = ComparableCmpError

        function compare(self, other: Self) -> int throws ComparableCmpError {
            if let a: int = self.value {
                if let b: int = other.value {
                    return a.compare(b)
                }
            }
            throw ComparableCmpError { message: "cannot compare null" }
        }
    }
}

function comparable_cmp_flaky(a: ComparableFlaky, b: ComparableFlaky) -> int throws ComparableCmpError {
    a.compare(b)
}

// The fallible comparator's happy path: both values present, delegates to
// the builtin `int.compare` from inside the impl body. The catch value is
// compared inline (a catch result stored in a test-block `let` stays boxed).
test "comparable_user_class_fallible_happy_path" {
    assert.is_true((comparable_cmp_flaky(ComparableFlaky { value: 1 }, ComparableFlaky { value: 2 }) catch (e) {
        ComparableCmpError => 99
    }) < 0)
}

test "comparable_user_class_custom_cmperror_propagates" {
    let msg = {
        let v = comparable_cmp_flaky(ComparableFlaky { value: 1 }, ComparableFlaky { value: null });
        "no throw"
    } catch (e) {
        ComparableCmpError => e.message
    };
    assert.is_true(msg.matches("cannot compare null"))
}

// ─── Sortable: `sort` (Phase 3 — new path alongside the old `sort`) ─────────
//
// Parity with the Phase 0 `sort()` corpus. These exercise `sort()` on *local*
// array receivers (`xs.sort()`).
//
// NB: these run the sort on a *local* receiver rather than through a
// `sort_ints(xs)` helper. Passing a test-block `let` local to a function
// delivers it as `Null` inside the callee — a pre-existing VM quirk of `test`
// blocks (see the `arrays.baml` header note), unrelated to sort. Param-receiver
// dispatch itself works in real functions (Rust `phase3_*` tests). The helper
// functions below are still *declared* (and compile) to pin the `throws never`
// normalization for parameter receivers at compile time.

function sort_ints(xs: int[]) -> int[] throws never { xs.sort() }
function sort_strings(xs: string[]) -> string[] throws never { xs.sort() }
function sort_bigints(xs: bigint[]) -> bigint[] throws never { xs.sort() }

test "sort_ints_in_place_returns_self" {
    let xs = [3, 1, 2];
    let result = xs.sort();
    let expected = [1, 2, 3];
    assert.is_true(result == xs);
    assert.is_true(baml.deep_equals(xs, expected))
}

test "sort_empty_is_noop" {
    let xs: int[] = [];
    let result = xs.sort();
    let expected: int[] = [];
    assert.is_true(result == xs);
    assert.is_true(baml.deep_equals(xs, expected))
}

test "sort_duplicates" {
    let xs = [3, 1, 2, 1, 3];
    xs.sort();
    let expected = [1, 1, 2, 3, 3];
    assert.is_true(baml.deep_equals(xs, expected))
}

test "sort_strings_match_sort" {
    let xs = ["b", "a", "aa"];
    xs.sort();
    let expected = ["a", "aa", "b"];
    assert.is_true(baml.deep_equals(xs, expected))
}

test "sort_bigints_match_sort" {
    let xs = [3n, 1n, 2n];
    xs.sort();
    let expected = [1n, 2n, 3n];
    assert.is_true(baml.deep_equals(xs, expected))
}

// float's SortError is InvalidArgument — fallible, declared not caught.
function sort_floats(xs: float[]) -> float[] throws baml.errors.InvalidArgument {
    xs.sort()
}

test "sort_floats_match_sort" {
    let xs = [2.5, 0.5, 1.5];
    let caught = {
        sort_floats(xs);
        false
    } catch (e) {
        baml.errors.InvalidArgument => true
    };
    let expected = [0.5, 1.5, 2.5];
    assert.is_true(baml.deep_equals(xs, expected))
}

test "sort_floats_nan_throws_and_rolls_back" {
    let xs = [1.0, float.nan(), 0.5];
    let msg = {
        sort_floats(xs);
        "no throw"
    } catch (e) {
        baml.errors.InvalidArgument => e.message
    };
    assert.is_true(msg.matches("NaN"));
    // rollback: pre-sort order preserved (NaN can't be equality-checked, so
    // assert the first element is still 1.0)
    assert.is_true(baml.deep_equals(xs.length(), 3));
    assert.equal(xs.at(0), 1.0)
}

// User class with an infallible (`never`) CmpError sorts by its compare —
// stably, in place, returning self.
class SortableItem {
    rank int
    label string
    implements baml.Comparable {
        type CmpError = never
        function compare(self, other: Self) -> int throws never {
            self.rank.compare(other.rank)
        }
    }
}

// `sort_items` is declared (and compiles) to pin the `throws never`
// normalization for an infallible user `Comparable` at compile time; the
// runtime tests below dispatch on local receivers (see the param-receiver
// limitation note above).
function sort_items(xs: SortableItem[]) -> SortableItem[] throws never {
    xs.sort()
}

test "sort_user_class_sorts_by_compare" {
    let xs = [
        SortableItem { rank: 3, label: "c" },
        SortableItem { rank: 1, label: "a" },
        SortableItem { rank: 2, label: "b" }
    ];
    let result = xs.sort();
    assert.is_true(result == xs);
    let labels = xs.map((x: SortableItem) -> string { x.label });
    let expected = ["a", "b", "c"];
    assert.is_true(baml.deep_equals(labels, expected))
}

test "sort_user_class_is_stable" {
    let xs = [
        SortableItem { rank: 2, label: "a" },
        SortableItem { rank: 1, label: "b" },
        SortableItem { rank: 2, label: "c" },
        SortableItem { rank: 1, label: "d" }
    ];
    xs.sort();
    let labels = xs.map((x: SortableItem) -> string { x.label });
    let expected = ["b", "d", "a", "c"];
    assert.is_true(baml.deep_equals(labels, expected))
}

// Fallible comparator (CmpError = ComparableCmpError): the call site must
// handle it, and a throw mid-sort leaves the array in pre-sort state.
function sort_flaky(xs: ComparableFlaky[]) -> ComparableFlaky[] throws ComparableCmpError {
    xs.sort()
}

test "sort_fallible_comparator_throw_rolls_back" {
    let xs = [
        ComparableFlaky { value: 3 },
        ComparableFlaky { value: null },
        ComparableFlaky { value: 1 }
    ];
    let msg = {
        sort_flaky(xs);
        "no throw"
    } catch (e) {
        ComparableCmpError => e.message
    };
    assert.is_true(msg.matches("cannot compare null"));
    // rollback: original order intact
    let vals = xs.map((x: ComparableFlaky) -> int {
        if let v: int = x.value { v } else { -1 }
    });
    let expected = [3, -1, 1];
    assert.is_true(baml.deep_equals(vals, expected))
}

test "sort_fallible_comparator_happy_path" {
    let xs = [
        ComparableFlaky { value: 3 },
        ComparableFlaky { value: 1 },
        ComparableFlaky { value: 2 }
    ];
    let caught = {
        sort_flaky(xs);
        false
    } catch (e) {
        ComparableCmpError => true
    };
    let vals = xs.map((x: ComparableFlaky) -> int {
        if let v: int = x.value { v } else { -1 }
    });
    let expected = [1, 2, 3];
    assert.is_true(baml.deep_equals(vals, expected))
}

// Generic propagation: a function over `U extends Comparable` may call
// `xs.sort()`, propagating `U.CmpError` symbolically. The declaration
// type-checking is the assertion (compile-time); the runtime sort is exercised
// on a local receiver below.
function sort_generic<U extends baml.Comparable>(xs: U[]) -> U[] throws U.CmpError {
    xs.sort()
}

test "sort_generic_propagation" {
    let xs = [3, 1, 2];
    xs.sort();
    let expected = [1, 2, 3];
    assert.is_true(baml.deep_equals(xs, expected))
}
