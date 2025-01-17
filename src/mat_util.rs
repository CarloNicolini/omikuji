use crate::Index;
use hashbrown::HashSet;
use itertools::Itertools;
use ndarray::ArrayViewMut1;
use num_traits::{Float, Num, Unsigned, Zero};
use ordered_float::NotNan;
use serde::{Deserialize, Serialize};
use sprs::{CsMatBase, CsMatI, CsVecViewI, SpIndex};
use std::fmt::Display;
use std::ops::{AddAssign, Deref, DerefMut, DivAssign};

pub type SparseVec = sprs::CsVecI<f32, Index>;
pub type SparseVecView<'a> = sprs::CsVecViewI<'a, f32, Index>;
pub type SparseMat = sprs::CsMatI<f32, Index, usize>;
pub type SparseMatView<'a> = sprs::CsMatViewI<'a, f32, Index, usize>;
pub type DenseVec = ndarray::Array1<f32>;
pub type DenseMat = ndarray::Array2<f32>;
pub type DenseMatViewMut<'a> = ndarray::ArrayViewMut2<'a, f32>;

/// A weight matrix of one-vs-all classifiers, can be stored in either dense or sparse format.
///
/// The matrix has dimensions (# of features) x (# of classes). Compare to storing the weights
/// as a (# of classes) x (# of features) matrix, this storage is more cache friendly when the
/// matrix is dense.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum WeightMat {
    Sparse(LilMat),
    Dense(DenseMat),
}

impl WeightMat {
    /// Compute dot product with a sparse vector after transposing.
    ///
    /// This is equivalent to dot(vec, mat).
    pub fn t_dot_vec(&self, vec: SparseVecView) -> DenseVec {
        match self {
            Self::Dense(mat) => mat.t().outer_iter().map(|w| vec.dot_dense(w)).collect(),
            Self::Sparse(mat) => mat.t_dot_csvec(vec),
        }
    }

    /// Get the shape of the matrix.
    pub fn shape(&self) -> sprs::Shape {
        match self {
            Self::Dense(mat) => {
                let shape = mat.shape();
                assert!(shape.len() == 2);
                (shape[0], shape[1])
            }
            Self::Sparse(mat) => mat.shape(),
        }
    }

    /// Returns whether the matrix is dense.
    pub fn is_dense(&self) -> bool {
        match self {
            Self::Dense(_) => true,
            Self::Sparse(_) => false,
        }
    }

    /// Returns the ratio of non-zero elements in the matrix when it's sparse.
    pub fn density(&self) -> f32 {
        match self {
            Self::Dense(_) => 1.,
            Self::Sparse(m) => m.density() as f32,
        }
    }

    /// Store the matrix in dense format if it's not already so.
    pub fn densify(&mut self) {
        *self = match self {
            Self::Dense(_) => {
                return; // Already dense, do nothing
            }
            Self::Sparse(m) => Self::Dense(m.to_dense()),
        };
    }

    /// Create a new matrix from sparse row vectors.
    ///
    /// By default the matrix is only stored in dense format if it takes up less memory than using
    /// the sparse format. One can call [`Self::densify()`] explicitly to force using the dense
    /// format, e.g., to trade size for speed.
    pub fn from_rows(row_vecs: &[SparseVec]) -> Self {
        let mat = LilMat::from_columns(row_vecs);
        let sparse_size = mat.mem_size();

        let (rows, cols) = mat.shape();
        let dense_size = std::mem::size_of::<f32>() * rows * cols;

        if dense_size <= sparse_size {
            Self::Dense(mat.to_dense())
        } else {
            Self::Sparse(mat)
        }
    }
}

pub trait IndexValuePairs<IndexT: SpIndex + Unsigned, ValueT: Copy>:
    Deref<Target = [(IndexT, ValueT)]>
{
    fn is_valid_sparse_vec(&self, length: usize) -> bool {
        // If empty, always valid
        if self.is_empty() {
            return true;
        }
        // Check if:
        // - All indices are smaller than max index
        // - Pairs are sorted by indices
        // - There are no duplicate indices
        if self[0].0.index() >= length {
            return false;
        }
        if self.len() > 1 {
            for ((i, _), (j, _)) in self.iter().skip(1).zip(self.iter()) {
                if i.index() >= length || i <= j {
                    return false;
                }
            }
        }

        true
    }
}

impl<IndexT, ValueT, PairsT> IndexValuePairs<IndexT, ValueT> for PairsT
where
    IndexT: SpIndex + Unsigned,
    ValueT: Copy,
    PairsT: Deref<Target = [(IndexT, ValueT)]>,
{
}

pub trait IndexValuePairsMut<IndexT, ValueT>: DerefMut<Target = [(IndexT, ValueT)]> {
    fn sort_by_index(&mut self)
    where
        IndexT: Ord,
    {
        self.sort_unstable_by(|l, r| l.0.cmp(&r.0));
    }

    fn l2_normalize(&mut self)
    where
        ValueT: Float + AddAssign + DivAssign,
    {
        let mut length = ValueT::zero();
        for (_, v) in self.iter() {
            length += v.powi(2);
        }

        if !length.is_zero() {
            length = length.sqrt();
            for (_, v) in self.iter_mut() {
                *v /= length;
            }
        }
    }
}

impl<IndexT, ValueT, PairsT> IndexValuePairsMut<IndexT, ValueT> for PairsT where
    PairsT: DerefMut<Target = [(IndexT, ValueT)]>
{
}

pub trait OwnedIndexValuePairs<IndexT, ValueT> {
    fn prune_with_threshold(&mut self, threshold: ValueT)
    where
        ValueT: Float;
}

impl<IndexT, ValueT> OwnedIndexValuePairs<IndexT, ValueT> for Vec<(IndexT, ValueT)> {
    fn prune_with_threshold(&mut self, threshold: ValueT)
    where
        ValueT: Float,
    {
        self.retain(|&(_, v)| v.abs() >= threshold);
    }
}

pub fn csrmat_from_index_value_pair_lists<IndexT, ValueT>(
    pair_lists: Vec<Vec<(IndexT, ValueT)>>,
    n_col: usize,
) -> sprs::CsMatI<ValueT, IndexT, usize>
where
    IndexT: SpIndex,
    ValueT: Copy,
{
    let n_row = pair_lists.len();
    let mut indptr: Vec<usize> = Vec::with_capacity(n_row + 1);
    let mut indices: Vec<IndexT> = Vec::new();
    let mut data: Vec<ValueT> = Vec::new();

    indptr.push(0);
    for row in pair_lists.into_iter() {
        for (i, v) in row.into_iter() {
            assert!(i.index() < n_col);
            indices.push(i);
            data.push(v);
        }
        indptr.push(indices.len());
    }

    sprs::CsMatI::new((n_row, n_col), indptr, indices, data)
}

pub trait CsMatBaseTools<DataT, IndexT: SpIndex, Iptr: SpIndex>: sprs::SparseMat {
    fn copy_outer_dims(&self, indices: &[usize]) -> CsMatI<DataT, IndexT, Iptr>;
}

impl<N, I, Iptr, IptrStorage, IndStorage, DataStorage> CsMatBaseTools<N, I, Iptr>
    for CsMatBase<N, I, IptrStorage, IndStorage, DataStorage, Iptr>
where
    I: SpIndex,
    N: Copy,
    IptrStorage: Deref<Target = [Iptr]>,
    IndStorage: Deref<Target = [I]>,
    DataStorage: Deref<Target = [N]>,
    Iptr: SpIndex,
{
    fn copy_outer_dims(&self, indices: &[usize]) -> CsMatI<N, I, Iptr> {
        let mut iptr = Vec::<Iptr>::with_capacity(indices.len() + 1);
        let mut ind = Vec::<I>::with_capacity(indices.len() * 2);
        let mut data = Vec::<N>::with_capacity(indices.len() * 2);

        iptr.push(Iptr::zero());
        for &i in indices {
            if let Some(v) = self.outer_view(i) {
                for &i in v.indices() {
                    ind.push(i);
                }
                for &v in v.data() {
                    data.push(v);
                }
            }

            iptr.push(
                Iptr::from::<usize>(ind.len()).unwrap_or_else(|| {
                    panic!("Failed to convert usize {} to index type", ind.len())
                }),
            );
        }

        CsMatI::new((indices.len(), self.inner_dims()), iptr, ind, data)
    }
}

pub trait CsMatITools<DataT: Copy, IndexT: SpIndex>: sprs::SparseMat + Sized {
    fn shrink_inner_indices(self) -> (Self, Vec<IndexT>);
    fn remap_inner_indices(self, old_index_to_new: &[IndexT], n_columns: usize) -> Self;
}

impl<N, I, Iptr> CsMatITools<N, I> for CsMatI<N, I, Iptr>
where
    I: SpIndex,
    N: Copy,
    Iptr: SpIndex,
{
    /// Shrinks inner indices of a Sparse matrix.
    ///
    /// The operation can be reversed by calling remap_inner_indices on the returned
    /// matrix and mapping.
    fn shrink_inner_indices(self) -> (Self, Vec<I>) {
        let new_index_to_old = {
            let mut old_indices = Vec::with_capacity(self.inner_dims());
            let mut index_set = HashSet::with_capacity(self.inner_dims());
            for &i in self.indices() {
                if index_set.insert(i.index()) {
                    old_indices.push(i);
                }
            }
            old_indices.sort_unstable();
            old_indices
        };

        let old_index_to_new = {
            let mut lookup = vec![I::zero(); self.inner_dims()];
            for (new_index, &old_index) in new_index_to_old.iter().enumerate() {
                lookup[old_index.index()] = I::from::<usize>(new_index).unwrap_or_else(|| {
                    panic!("Failed to convert usize {} to index type", new_index)
                });
            }
            lookup
        };

        let mat = self.remap_inner_indices(&old_index_to_new, new_index_to_old.len());
        (mat, new_index_to_old)
    }

    /// Remap inner indices according to the given mapping.
    ///
    /// The mapping is assumed to be well-formed, i.e. sorted, within range, and without duplicates.
    fn remap_inner_indices(self, old_index_to_new: &[I], new_inner_dims: usize) -> Self {
        let outer_dims = self.outer_dims();
        let is_csr = self.is_csr();

        let (indptr, mut indices, data) = self.into_raw_storage();
        for index in &mut indices {
            *index = old_index_to_new[index.index()];
        }
        let new_mat = CsMatI::new((outer_dims, new_inner_dims), indptr, indices, data);
        if is_csr {
            new_mat
        } else {
            new_mat.transpose_into()
        }
    }
}

pub fn csvec_dot_self<N, I>(vec: &CsVecViewI<N, I>) -> N
where
    I: SpIndex,
    N: Num + AddAssign + Copy,
{
    let mut prod = N::zero();
    for &val in vec.data() {
        prod += val * val;
    }
    prod
}

pub fn dense_add_assign_csvec<N, I>(mut dense_vec: ArrayViewMut1<N>, csvec: CsVecViewI<N, I>)
where
    I: sprs::SpIndex,
    N: Num + Copy + AddAssign,
{
    assert_eq!(dense_vec.len(), csvec.dim());
    for (i, &v) in csvec.iter() {
        // This is safe because we checked length above
        unsafe {
            *dense_vec.uget_mut(i) += v;
        }
    }
}

pub fn dense_add_assign_csvec_mul_scalar<N, I>(
    mut dense_vec: ArrayViewMut1<N>,
    csvec: CsVecViewI<N, I>,
    scalar: N,
) where
    I: sprs::SpIndex,
    N: Num + Copy + AddAssign,
{
    assert_eq!(dense_vec.len(), csvec.dim());
    for (i, &v) in csvec.iter() {
        // This is safe because we checked length above
        unsafe {
            *dense_vec.uget_mut(i) += v * scalar;
        }
    }
}

pub fn dense_vec_l2_normalize<N>(mut vec: ArrayViewMut1<N>)
where
    N: Float + DivAssign + ndarray::ScalarOperand,
{
    let length = vec.dot(&vec).sqrt();
    if length > N::from(1e-5).unwrap() {
        vec /= length;
    } else {
        vec.fill(N::zero());
    }
}

pub fn find_max<N>(arr: ndarray::ArrayView1<N>) -> Option<(N, usize)>
where
    N: Float + Display,
{
    if let Some((i, &v)) = arr
        .indexed_iter()
        .max_by_key(|(_, &l)| NotNan::new(l).unwrap())
    {
        Some((v, i))
    } else {
        None
    }
}

/// A sparse matrix stored in a compact list-of-lists format.
///
/// # Storage format
///
/// In the general case the storage could be either row- or column-major. In this implementation,
/// data is stored row-major, i.e., `outer_inds` and `inner_inds` store row and column
/// indices, respectively. Specifically, the matrix has `indptr.len() - 1` non-empty rows.
/// The `i`-th non-empty row has index `outer_inds[i]`, and the non-zero values in that row
/// have column indices `inner_inds[indptr[i]..indptr[i + 1]]` and corresponding values
/// `data[indptr[i]..indptr[i+1]]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LilMat {
    outer_dim: usize,
    inner_dim: usize,
    indptr: Vec<usize>,
    outer_inds: Vec<Index>,
    inner_inds: Vec<Index>,
    data: Vec<f32>,
}

impl LilMat {
    /// Create an all-zero matrix of the given shape.
    ///
    /// The current implementation assumes outer dimension to be columns, and inner to be rows.
    pub fn new(shape: sprs::Shape) -> Self {
        LilMat {
            outer_dim: shape.0,
            inner_dim: shape.1,
            indptr: vec![0],
            outer_inds: Vec::new(),
            inner_inds: Vec::new(),
            data: Vec::new(),
        }
    }

    /// Create an all zero matrix with the given shape and capacity.
    ///
    /// `nnz_outer` is the estimated number of columns with non-zero outer dimensions, and
    /// `nnz` is the estimated total number of non-zero elements.
    pub fn with_capacity(shape: sprs::Shape, nnz_outer: usize, nnz: usize) -> Self {
        let mut indptr = Vec::with_capacity(nnz_outer + 1);
        indptr.push(0);

        LilMat {
            outer_dim: shape.0,
            inner_dim: shape.1,
            indptr,
            outer_inds: Vec::with_capacity(nnz_outer),
            inner_inds: Vec::with_capacity(nnz),
            data: Vec::with_capacity(nnz),
        }
    }

    /// Create a new matrix from sparse column vectors.
    pub fn from_columns(col_vecs: &[SparseVec]) -> Self {
        if col_vecs.is_empty() {
            return Self::new((0, 0));
        }

        let (cols, rows) = (col_vecs.len(), col_vecs[0].dim());

        let mut triplets = Vec::new();
        let mut max_col_nnz = 0;
        let mut nnz = 0;
        for (col, vec) in col_vecs.iter().enumerate() {
            assert_eq!(
                rows,
                vec.dim(),
                "Unexpected row vector dimension {}; expected {}",
                rows,
                vec.dim()
            );
            max_col_nnz = max_col_nnz.max(vec.nnz());
            nnz += vec.nnz();
            for (row, &val) in vec.iter() {
                triplets.push((row, col, val));
            }
        }

        triplets.sort_unstable_by_key(|&(r, c, _)| (r, c));

        let mut mat = Self::with_capacity((rows, cols), max_col_nnz, nnz);
        for (row, col, val) in triplets {
            mat.append_value(row, col, val);
        }
        mat
    }

    /// Get the shape of the matrix.
    ///
    /// Note that here we assume the matrix is stored column-first, so the outer dimension is
    /// the column, and the inner dimmension is the row.
    pub fn shape(&self) -> sprs::Shape {
        (self.outer_dim, self.inner_dim)
    }

    /// The density of the sparse matrix, defined as the number of non-zero
    /// elements divided by the maximum number of elements
    pub fn density(&self) -> f64 {
        use sprs::SparseMat;
        let (rows, cols) = self.shape();
        if rows.is_zero() && cols.is_zero() {
            f64::nan()
        } else {
            self.nnz() as f64 / (rows * cols) as f64
        }
    }

    /// Append a new value to the matrix.
    ///
    /// The function should be called in non-descending order of outer index and ascending order
    /// of inner index.
    pub fn append_value(&mut self, outer_ind: usize, inner_ind: usize, value: f32) {
        if value.is_zero() {
            return;
        }
        assert!(outer_ind < self.outer_dim, "Outer index out of range");
        assert!(inner_ind < self.inner_dim, "Inner index out of range");

        let (outer_ind, inner_ind) = (Index::from_usize(outer_ind), Index::from_usize(inner_ind));

        // When either the matrix is empty, or the last outer index is strictly less than
        // the new one, we are appending to a new outer index.
        if self.outer_inds.last().map_or(true, |&i| i < outer_ind) {
            self.outer_inds.push(outer_ind);
            self.indptr.push(self.inner_inds.len());
        } else {
            // Otherwise we should be appending to the same outer index as the last value. Here we
            // check whether indices are appended out of order.
            assert!(
                *self.outer_inds.last().unwrap() == outer_ind,
                "Outer index {} out of order",
                outer_ind
            );
            assert!(
                *self.inner_inds.last().unwrap() < inner_ind,
                "Inner index {} out of order",
                inner_ind
            );
        }

        self.inner_inds.push(inner_ind);
        self.data.push(value);
        *self.indptr.last_mut().unwrap() += 1;

        debug_assert_eq!(self.indptr.len(), self.outer_inds.len() + 1);
        debug_assert_eq!(self.inner_inds.len(), self.data.len());
        debug_assert!(
            self.indptr.len() > 1
                && self.indptr.last().unwrap().index_unchecked() == self.data.len()
        );
    }

    /// Assign non-zero values to a dense matrix.
    pub fn assign_to_dense(&self, mut array: DenseMatViewMut) {
        for ((&ind_l, &ind_r), &outer_ind) in self
            .indptr
            .iter()
            .zip(self.indptr.iter().skip(1))
            .zip_eq(self.outer_inds.iter())
        {
            let (ind_l, ind_r, outer_ind) = (
                ind_l.index_unchecked(),
                ind_r.index_unchecked(),
                outer_ind.index_unchecked(),
            );
            let inner_inds = &self.inner_inds[ind_l..ind_r];
            let data = &self.data[ind_l..ind_r];
            for (&inner_ind, &value) in inner_inds.iter().zip(data.iter()) {
                let inner_ind = inner_ind.index_unchecked();
                array[[outer_ind, inner_ind]] = value;
            }
        }
    }

    /// Convert to dense format.
    pub fn to_dense(&self) -> DenseMat {
        let mut dense_mat = DenseMat::zeros(self.shape());
        self.assign_to_dense(dense_mat.view_mut());
        dense_mat
    }

    /// The size in memory in bytes.
    pub fn mem_size(&self) -> usize {
        std::mem::size_of_val(self.indptr.as_slice())
            + std::mem::size_of_val(self.outer_inds.as_slice())
            + std::mem::size_of_val(self.inner_inds.as_slice())
            + std::mem::size_of_val(self.data.as_slice())
    }

    /// Compute dot product with a sparse vector after transposing.
    ///
    /// The implementation uses binary search on row (column after transposing) indices.
    pub fn t_dot_csvec(&self, vec: SparseVecView) -> DenseVec {
        let (t_cols, t_rows) = self.shape();
        assert_eq!(
            t_cols,
            vec.dim(),
            "Dimension mismatch: {} != {}",
            t_cols,
            vec.dim()
        );
        let mut out = DenseVec::zeros(t_rows);

        let mut i = 0; // i marks the next matrix outer index from which to binary search
        for (outer_idx, &val1) in vec.iter() {
            // NB:
            //  Since the binary search is done on the slice [i..], the returned index di is an
            //  offset from i.
            let (di, found) =
                match self.outer_inds[i..].binary_search(&Index::from_usize(outer_idx)) {
                    Ok(di) => (di, true),
                    Err(di) => (di, false),
                };
            i += di;
            if found {
                let rng = self.indptr[i].index_unchecked()..self.indptr[i + 1].index_unchecked();
                for (&inner_idx, &val2) in self.inner_inds[rng.clone()]
                    .iter()
                    .zip_eq(self.data[rng.clone()].iter())
                {
                    out[inner_idx.index_unchecked()] += val1 * val2;
                }
            }
        }

        out
    }
}

impl sprs::SparseMat for LilMat {
    fn rows(&self) -> usize {
        self.outer_dim
    }

    fn cols(&self) -> usize {
        self.inner_dim
    }

    fn nnz(&self) -> usize {
        self.data.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndarray::array;
    use sprs::CsVecI;

    #[test]
    fn test_is_valid_sparse_vec() {
        assert!(Vec::<(usize, f64)>::new().is_valid_sparse_vec(0));
        assert!(Vec::<(usize, f64)>::new().is_valid_sparse_vec(123));

        assert!(vec![(123u32, 123.)].is_valid_sparse_vec(124));
        assert!(!vec![(123u32, 123.)].is_valid_sparse_vec(123));

        assert!(vec![(1u32, 0.), (3, 0.), (5, 0.)].is_valid_sparse_vec(6));
        assert!(!vec![(1u32, 0.), (3, 0.), (5, 0.)].is_valid_sparse_vec(5));
        assert!(!vec![(1u32, 0.), (5, 0.), (3, 0.)].is_valid_sparse_vec(6));
    }

    #[test]
    fn test_sort_by_index() {
        let mut pairs = vec![(1, 123.), (3, 321.), (2, 213.), (4, 432.)];
        pairs.sort_by_index();
        assert_eq!(vec![(1, 123.), (2, 213.), (3, 321.), (4, 432.)], pairs);
    }

    #[test]
    fn test_l2_normalize() {
        let mut pairs = vec![(1, 1.), (5, 2.), (50, 4.), (100, 6.), (1000, 8.)];
        pairs.l2_normalize();
        assert_eq!(
            vec![
                (1, 1. / 11.),
                (5, 2. / 11.),
                (50, 4. / 11.),
                (100, 6. / 11.),
                (1000, 8. / 11.),
            ],
            pairs
        );

        let mut pairs = vec![(1, 0.), (5, 0.), (50, 0.), (100, 0.), (1000, 0.)];
        pairs.l2_normalize();
        assert_eq!(
            vec![(1, 0.), (5, 0.), (50, 0.), (100, 0.), (1000, 0.),],
            pairs
        );
    }

    #[test]
    fn test_prune_with_threshold() {
        let mut v = vec![(1, 0.0001), (5, 0.001), (50, 0.01), (100, -0.1)];
        v.prune_with_threshold(0.01);
        assert_eq!(vec![(50, 0.01), (100, -0.1)], v);
    }

    #[test]
    fn test_csrmat_from_index_value_pair_lists() {
        let mat = vec![
            vec![(0usize, 1), (1, 2)],
            vec![(0, 3), (2, 4)],
            vec![(2, 5)],
        ];
        assert_eq!(
            sprs::CsMat::new(
                (3, 5),
                vec![0, 2, 4, 5],
                vec![0, 1, 0, 2, 2],
                vec![1, 2, 3, 4, 5],
            ),
            csrmat_from_index_value_pair_lists(mat, 5)
        );
    }

    #[test]
    fn test_copy_outer_dims() {
        let mat = sprs::CsMat::new(
            (3, 3),
            vec![0, 2, 4, 5],
            vec![0, 1, 0, 2, 2],
            vec![1, 2, 3, 4, 5],
        );
        assert_eq!(
            sprs::CsMat::new(
                (4, 3),
                vec![0, 2, 3, 3, 5],
                vec![0, 1, 2, 0, 2],
                vec![1, 2, 5, 3, 4],
            ),
            mat.copy_outer_dims(&[0, 2, 3, 1])
        );
    }

    #[test]
    fn test_remap_inner_indices() {
        let mat = sprs::CsMat::new(
            (3, 3),
            vec![0, 2, 4, 5],
            vec![0, 1, 0, 2, 2],
            vec![1, 2, 3, 4, 5],
        );
        let expected_mat = sprs::CsMat::new(
            (3, 2000),
            vec![0, 2, 4, 5],
            vec![10, 100, 10, 1000, 1000],
            vec![1, 2, 3, 4, 5],
        );

        assert_eq!(
            expected_mat.clone(),
            mat.clone().remap_inner_indices(&vec![10, 100, 1000], 2000)
        );
        assert_eq!(
            expected_mat.transpose_into(),
            mat.transpose_into()
                .remap_inner_indices(&vec![10, 100, 1000], 2000)
        );
    }

    #[test]
    fn test_shrink_inner_indices() {
        let mat = sprs::CsMat::new(
            (3, 2000),
            vec![0, 2, 4, 5],
            vec![10, 100, 10, 1000, 1000],
            vec![1, 2, 3, 4, 5],
        );
        let expected_mat = sprs::CsMat::new(
            (3, 3),
            vec![0, 2, 4, 5],
            vec![0, 1, 0, 2, 2],
            vec![1, 2, 3, 4, 5],
        );
        assert_eq!(
            (expected_mat.clone(), vec![10, 100, 1000]),
            mat.clone().shrink_inner_indices()
        );
        assert_eq!(
            (expected_mat.transpose_into(), vec![10, 100, 1000]),
            mat.transpose_into().shrink_inner_indices()
        );
    }

    #[test]
    fn test_dense_add_assign_csvec() {
        let mut dense = array![1, 2, 3, 4, 5];
        let sparse = CsVecI::new(5, vec![1, 3], vec![6, 7]);
        dense_add_assign_csvec(dense.view_mut(), sparse.view());
        assert_eq!(array![1, 2 + 6, 3, 4 + 7, 5], dense);
    }

    #[test]
    fn test_dense_add_assign_csvec_mul_scalar() {
        let mut dense = array![1, 2, 3, 4, 5];
        let sparse = CsVecI::new(5, vec![1, 3], vec![6, 7]);
        dense_add_assign_csvec_mul_scalar(dense.view_mut(), sparse.view(), 2);
        assert_eq!(array![1, 2 + 6 * 2, 3, 4 + 7 * 2, 5], dense);
    }

    #[test]
    fn test_dense_vec_l2_normalize() {
        let mut v = array![1., 2., 4., 6., 8.];
        dense_vec_l2_normalize(v.view_mut());
        assert_eq!(array![1. / 11., 2. / 11., 4. / 11., 6. / 11., 8. / 11.], v);
    }

    #[test]
    fn test_find_max() {
        assert_eq!(Some((3., 0)), find_max(array![3.].view()));
        assert_eq!(
            Some((10., 4)),
            find_max(array![3., 5., 1., 5., 10., 0.].view())
        );
        assert_eq!(None, find_max(DenseVec::zeros(0).view()));
    }

    #[test]
    fn test_lil_mat_density() {
        let mat = LilMat::from_columns(&vec![
            SparseVec::new(5, vec![1, 3], vec![1., 3.]),
            SparseVec::new(5, vec![0], vec![2.]),
            SparseVec::new(5, vec![], vec![]),
            SparseVec::new(5, vec![2, 3], vec![4., 5.]),
        ]);
        assert_eq!(5. / (4. * 5.), mat.density())
    }

    #[test]
    fn test_lil_mat_construction_and_to_dense() {
        let mut mat = LilMat::new((4, 5));
        let mut array = DenseMat::zeros((4, 5));

        {
            assert!(mat.to_dense().iter().all(|&v| v == 0.0));
            mat.assign_to_dense(array.view_mut());
            assert!(array.iter().all(|&v| v == 0.0));
        }

        {
            mat.append_value(0, 1, 2.0);
            mat.append_value(1, 0, 1.0);
            mat.append_value(2, 3, 4.0);
            mat.append_value(3, 0, 3.0);
            mat.append_value(3, 3, 5.0);

            let expected_array = array![
                [0, 2, 0, 0, 0],
                [1, 0, 0, 0, 0],
                [0, 0, 0, 4, 0],
                [3, 0, 0, 5, 0]
            ]
            .map(|&v| v as f32);

            assert_eq!(expected_array, mat.to_dense());

            mat.assign_to_dense(array.view_mut());
            assert_eq!(expected_array, array);

            assert_eq!(
                expected_array,
                LilMat::from_columns(&vec![
                    SparseVec::new(4, vec![1, 3], vec![1., 3.]),
                    SparseVec::new(4, vec![0], vec![2.]),
                    SparseVec::new(4, vec![], vec![]),
                    SparseVec::new(4, vec![2, 3], vec![4., 5.]),
                    SparseVec::new(4, vec![], vec![]),
                ])
                .to_dense()
            );
        }
    }

    #[test]
    fn test_lil_mat_t_dot_csvec() {
        let csvec = SparseVec::new(4, vec![0, 2, 3], vec![1., 2., 3.]); // [1, 0, 2, 3]
        let mut mat = LilMat::new((4, 5));
        assert_eq!(array![0., 0., 0., 0., 0.], mat.t_dot_csvec(csvec.view()));

        /*
           [[0, 1, 0, 3, 0],
           [2, 0, 0, 0, 0],
           [0, 0, 0, 0, 0],
           [0, 0, 4, 5, 0]]
        */
        mat.append_value(0, 1, 1.);
        mat.append_value(0, 3, 3.);
        mat.append_value(1, 0, 2.);
        mat.append_value(3, 2, 4.);
        mat.append_value(3, 3, 5.);

        assert_eq!(
            array![0., 1., 3. * 4., 3. * 1. + 5. * 3., 0.],
            mat.t_dot_csvec(csvec.view())
        );
    }
}
