/// A total natural ordering for values of the implementing type.
///
/// `compare` follows the `sort_by` comparator convention: negative means
/// `self` orders before `other`, zero preserves relative order, positive
/// means `self` orders after `other`.
///
/// `CmpError` is the error a single comparison may throw. A total order that
/// never fails binds `type CmpError = never`; a fallible comparator binds
/// `type CmpError = MyError` and the error propagates to whoever compares (or
/// sorts) the values.
///
/// ```
/// class Resume {
///   name string
///   implements Comparable {
///     type CmpError = never
///     function compare(self, other: Self) -> int throws never {
///       self.name.compare(other.name)
///     }
///   }
/// }
/// ```
///
/// Note: `CmpError` is intentionally *not* defaulted. A defaulted associated
/// type would be injected into a bare `T extends Comparable` bound as the
/// concrete default, which then rejects any implementor that overrides it —
/// so `MyError[].sort()` (a fallible comparator) would stop compiling. Leaving
/// it undefaulted keeps the `Comparable` bound permissive over `CmpError`.
interface Comparable {
  type CmpError

  function compare(self, other: Self) -> int throws CmpError
}

implements Comparable for int {
  type CmpError = never
  function compare(self, other: Self) -> int throws never {
    if (self < other) { -1 } else if (self > other) { 1 } else { 0 }
  }
}

implements Comparable for bigint {
  type CmpError = never
  function compare(self, other: Self) -> int throws never {
    if (self < other) { -1 } else if (self > other) { 1 } else { 0 }
  }
}

implements Comparable for string {
  type CmpError = never
  function compare(self, other: Self) -> int throws never {
    if (self < other) { -1 } else if (self > other) { 1 } else { 0 }
  }
}

/// Sorting for collections whose elements have a natural order.
///
/// Implemented for `T[]` whenever `T implements Comparable`. `SortError` is
/// the element's `CmpError`: sorting an `int[]` or `string[]` can never
/// fail, sorting a `float[]` can throw `InvalidArgument` (NaN), and sorting
/// a user type with a fallible comparator propagates that comparator's
/// error.
interface Sortable {
  type SortError

  function sort(self) -> Self throws SortError
}

implements<T extends Comparable> Sortable for T[] {
  type SortError = T.CmpError

  /// Sorts this array in place by the elements' natural order
  /// (`Comparable.compare`), then returns `self`. The sort is stable. If a
  /// comparison throws mid-sort, the array is left in its pre-sort state.
  ///
  /// Implemented natively (`bex_vm::package_baml::array::sort_comparable_native`):
  /// primitive-domain arrays use the native natural-order sort; user
  /// `Comparable` types dispatch `compare` per pair by runtime type. A BAML
  /// body delegating to `sort_by` would route the per-element `compare`
  /// through the generic interface-dispatch machinery, which does not survive
  /// a native higher-order callback, so this is a `$rust_function`.
  //baml:mut_vm
  //baml:may_yield
  //baml:mut_self
  function sort(self) -> Self throws T.CmpError {
    $rust_function
  }
}

// Floats have no total natural order: NaN is unordered against everything,
// so `compare` rejects it. This preserves the pre-Comparable `sort()`
// semantics ("natural ordering does not support NaN") while letting the
// NaN-free comparisons remain ordinary three-way compares.
implements Comparable for float {
  type CmpError = root.errors.InvalidArgument

  function compare(self, other: Self) -> int throws root.errors.InvalidArgument {
    if (self.is_nan() || other.is_nan()) {
      throw root.errors.InvalidArgument {
        message: "float.compare: natural ordering does not support NaN"
      }
    }
    if (self < other) { -1 } else if (self > other) { 1 } else { 0 }
  }
}
