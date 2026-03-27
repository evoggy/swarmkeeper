/// IPPE (Infinitesimal Plane-Based Pose Estimation) solver
///
/// Ported from _ippe.py, based on:
/// Collins, Toby and Bartoli, Adrien, "Infinitesimal Plane-Based Pose Estimation",
/// International Journal of Computer Vision, 2014
///
/// This code uses OpenCV coordinate conventions internally.

use nalgebra::{DMatrix, DVector, Matrix3, Matrix4, Vector3, Vector4};

#[derive(Debug, Clone)]
pub(crate) struct IppeSolution {
    pub r: Matrix3<f64>,
    pub t: Vector3<f64>,
    pub reproj_err: f64,
}

/// Run IPPE with 2D model points (on z=0 plane) and 2D normalized image points.
/// u_points: Nx2 model points (world coords on z=0)
/// q_points: Nx2 normalized image points
pub(crate) fn mat_run_2d(u_points: &[[f64; 2]], q_points: &[[f64; 2]]) -> Option<[IppeSolution; 2]> {
    let n = u_points.len();
    if n < 4 || q_points.len() != n {
        return None;
    }

    let mut u = DMatrix::zeros(2, n);
    let q = DMatrix::from_fn(2, n, |r, c| q_points[c][r]);
    for j in 0..n {
        u[(0, j)] = u_points[j][0];
        u[(1, j)] = u_points[j][1];
    }

    let u_uncentred = u.clone();

    // Zero-center
    let pbar_x = u.row(0).mean();
    let pbar_y = u.row(1).mean();
    for j in 0..n {
        u[(0, j)] -= pbar_x;
        u[(1, j)] -= pbar_y;
    }

    let (r1, r2, t1_, t2_) = solve_core(&u, &q)?;

    // Apply centering correction
    let neg_pbar = Vector3::new(-pbar_x, -pbar_y, 0.0);
    let t1 = r1 * neg_pbar + t1_;
    let t2 = r2 * neg_pbar + t2_;

    let (err1, err2) = compute_reproj_errs_2d(&r1, &r2, &t1, &t2, &u_uncentred, &q);
    Some(sort_solutions(r1, r2, t1, t2, err1, err2))
}

/// Run IPPE with 3D model points (will be rotated onto z=0 plane) and 2D normalized image points.
/// u_points: Nx3 model points
/// q_points: Nx2 normalized image points
pub(crate) fn mat_run_3d(u_points: &[[f64; 3]], q_points: &[[f64; 2]]) -> Option<[IppeSolution; 2]> {
    let n = u_points.len();
    if n < 4 || q_points.len() != n {
        return None;
    }

    // Build 3xN matrix
    let u3 = DMatrix::from_fn(3, n, |r, c| u_points[c][r]);
    let q = DMatrix::from_fn(2, n, |r, c| q_points[c][r]);

    let u_uncentred_3d = u3.clone();

    // Center the 3D points
    let pbar = Vector3::new(
        u3.row(0).mean(),
        u3.row(1).mean(),
        u3.row(2).mean(),
    );

    // Build centering matrix MCenter (4x4 identity with -Pbar in last column)
    let mut m_center = Matrix4::identity();
    m_center[(0, 3)] = -pbar[0];
    m_center[(1, 3)] = -pbar[1];
    m_center[(2, 3)] = -pbar[2];

    // U_ = MCenter[0:3, :] * [U; ones]
    let mut u_h = DMatrix::zeros(4, n);
    for j in 0..n {
        u_h[(0, j)] = u3[(0, j)];
        u_h[(1, j)] = u3[(1, j)];
        u_h[(2, j)] = u3[(2, j)];
        u_h[(3, j)] = 1.0;
    }

    // Take top 3 rows of MCenter
    let m_center_3x4 = m_center.fixed_rows::<3>(0);
    let u_centered = m_center_3x4 * &u_h;

    // SVD of U_ * U_^T to find the plane rotation
    let uut = &u_centered * u_centered.transpose();
    let svd = uut.svd(true, false);
    let model_rotation_t = svd.u?; // 3x3

    // Build 4x4 model rotation (transposed, as in Python: modelRotation = modelRotation.T)
    let mut model_rot_4x4 = Matrix4::zeros();
    for i in 0..3 {
        for j in 0..3 {
            model_rot_4x4[(i, j)] = model_rotation_t[(j, i)]; // transpose
        }
    }
    model_rot_4x4[(3, 3)] = 1.0;

    // Mcorrective = modelRotation * MCenter
    let m_corrective = model_rot_4x4 * m_center;

    // U_2d = Mcorrective[0:2, :] * [U3d; ones]
    let mut u = DMatrix::zeros(2, n);
    for j in 0..n {
        for i in 0..2 {
            u[(i, j)] = m_corrective[(i, 0)] * u3[(0, j)]
                + m_corrective[(i, 1)] * u3[(1, j)]
                + m_corrective[(i, 2)] * u3[(2, j)]
                + m_corrective[(i, 3)];
        }
    }

    let (r1_2d, r2_2d, t1_2d, t2_2d) = solve_core(&u, &q)?;

    // Undo the corrective transformation:
    // M = [R | t; 0 0 0 1] * Mcorrective
    let mut m1 = Matrix4::zeros();
    let mut m2 = Matrix4::zeros();
    for i in 0..3 {
        for j in 0..3 {
            m1[(i, j)] = r1_2d[(i, j)];
            m2[(i, j)] = r2_2d[(i, j)];
        }
        m1[(i, 3)] = t1_2d[i];
        m2[(i, 3)] = t2_2d[i];
    }
    m1[(3, 3)] = 1.0;
    m2[(3, 3)] = 1.0;

    let m1 = m1 * m_corrective;
    let m2 = m2 * m_corrective;

    let r1 = m1.fixed_view::<3, 3>(0, 0).into_owned();
    let r2 = m2.fixed_view::<3, 3>(0, 0).into_owned();
    let t1 = Vector3::new(m1[(0, 3)], m1[(1, 3)], m1[(2, 3)]);
    let t2 = Vector3::new(m2[(0, 3)], m2[(1, 3)], m2[(2, 3)]);

    let (err1, err2) = compute_reproj_errs_3d(&r1, &r2, &t1, &t2, &u_uncentred_3d, &q);
    Some(sort_solutions(r1, r2, t1, t2, err1, err2))
}

fn sort_solutions(
    r1: Matrix3<f64>, r2: Matrix3<f64>,
    t1: Vector3<f64>, t2: Vector3<f64>,
    err1: f64, err2: f64,
) -> [IppeSolution; 2] {
    if err1 <= err2 {
        [
            IppeSolution { r: r1, t: t1, reproj_err: err1 },
            IppeSolution { r: r2, t: t2, reproj_err: err2 },
        ]
    } else {
        [
            IppeSolution { r: r2, t: t2, reproj_err: err2 },
            IppeSolution { r: r1, t: t1, reproj_err: err1 },
        ]
    }
}

/// Core solver: given centered 2D model points and 2D image points, compute R and t
fn solve_core(
    u: &DMatrix<f64>,
    q: &DMatrix<f64>,
) -> Option<(Matrix3<f64>, Matrix3<f64>, Vector3<f64>, Vector3<f64>)> {
    let n = u.ncols();

    // Build homogeneous coordinates
    let mut u_h = DMatrix::zeros(3, n);
    let mut q_h = DMatrix::zeros(3, n);
    for j in 0..n {
        u_h[(0, j)] = u[(0, j)];
        u_h[(1, j)] = u[(1, j)];
        u_h[(2, j)] = 1.0;
        q_h[(0, j)] = q[(0, j)];
        q_h[(1, j)] = q[(1, j)];
        q_h[(2, j)] = 1.0;
    }

    let h = homography2d(&u_h, &q_h)?;

    let h22 = h[(2, 2)];
    if h22.abs() < 1e-15 {
        return None;
    }
    let h = h / h22;

    // Jacobian of homography at (0,0)
    let j00 = h[(0, 0)] - h[(2, 0)] * h[(0, 2)];
    let j01 = h[(0, 1)] - h[(2, 1)] * h[(0, 2)];
    let j10 = h[(1, 0)] - h[(2, 0)] * h[(1, 2)];
    let j11 = h[(1, 1)] - h[(2, 1)] * h[(1, 2)];

    let v = [h[(0, 2)], h[(1, 2)]];
    let j_mat = [[j00, j01], [j10, j11]];

    let (r1, r2, _gamma) = ippe_dec(&v, &j_mat);
    let t1 = est_t(&r1, u, q);
    let t2 = est_t(&r2, u, q);

    Some((r1, r2, t1, t2))
}

/// Compute reprojection errors for 2D model points
fn compute_reproj_errs_2d(
    r1: &Matrix3<f64>, r2: &Matrix3<f64>,
    t1: &Vector3<f64>, t2: &Vector3<f64>,
    u: &DMatrix<f64>, q: &DMatrix<f64>,
) -> (f64, f64) {
    let n = u.ncols();
    let mut err1_sq = 0.0;
    let mut err2_sq = 0.0;

    for j in 0..n {
        let ux = u[(0, j)];
        let uy = u[(1, j)];

        let px1 = r1[(0, 0)] * ux + r1[(0, 1)] * uy + t1[0];
        let py1 = r1[(1, 0)] * ux + r1[(1, 1)] * uy + t1[1];
        let pz1 = r1[(2, 0)] * ux + r1[(2, 1)] * uy + t1[2];

        let px2 = r2[(0, 0)] * ux + r2[(0, 1)] * uy + t2[0];
        let py2 = r2[(1, 0)] * ux + r2[(1, 1)] * uy + t2[1];
        let pz2 = r2[(2, 0)] * ux + r2[(2, 1)] * uy + t2[2];

        let dx1 = px1 / pz1 - q[(0, j)];
        let dy1 = py1 / pz1 - q[(1, j)];
        let dx2 = px2 / pz2 - q[(0, j)];
        let dy2 = py2 / pz2 - q[(1, j)];

        err1_sq += dx1 * dx1 + dy1 * dy1;
        err2_sq += dx2 * dx2 + dy2 * dy2;
    }

    (err1_sq.sqrt(), err2_sq.sqrt())
}

/// Compute reprojection errors for 3D model points
fn compute_reproj_errs_3d(
    r1: &Matrix3<f64>, r2: &Matrix3<f64>,
    t1: &Vector3<f64>, t2: &Vector3<f64>,
    u: &DMatrix<f64>, q: &DMatrix<f64>,
) -> (f64, f64) {
    let n = u.ncols();
    let mut err1_sq = 0.0;
    let mut err2_sq = 0.0;

    for j in 0..n {
        let p = Vector3::new(u[(0, j)], u[(1, j)], u[(2, j)]);
        let p1 = r1 * p + t1;
        let p2 = r2 * p + t2;

        let dx1 = p1[0] / p1[2] - q[(0, j)];
        let dy1 = p1[1] / p1[2] - q[(1, j)];
        let dx2 = p2[0] / p2[2] - q[(0, j)];
        let dy2 = p2[1] / p2[2] - q[(1, j)];

        err1_sq += dx1 * dx1 + dy1 * dy1;
        err2_sq += dx2 * dx2 + dy2 * dy2;
    }

    (err1_sq.sqrt(), err2_sq.sqrt())
}

/// Estimate translation given rotation and point correspondences
fn est_t(r: &Matrix3<f64>, ps_plane: &DMatrix<f64>, q: &DMatrix<f64>) -> Vector3<f64> {
    let n = ps_plane.ncols();

    // Extend 2D to 3D with z=0
    let mut ps3 = DMatrix::zeros(3, n);
    for j in 0..n {
        ps3[(0, j)] = ps_plane[(0, j)];
        ps3[(1, j)] = ps_plane[(1, j)];
    }

    let ps = r * ps3;

    let mut a = DMatrix::zeros(2 * n, 3);
    let mut b = DVector::zeros(2 * n);

    for i in 0..n {
        a[(i, 0)] = 1.0;
        a[(i, 2)] = -q[(0, i)];
        b[i] = q[(0, i)] * ps[(2, i)] - ps[(0, i)];

        a[(n + i, 1)] = 1.0;
        a[(n + i, 2)] = -q[(1, i)];
        b[n + i] = q[(1, i)] * ps[(2, i)] - ps[(1, i)];
    }

    let ata = a.transpose() * &a;
    let atb = a.transpose() * &b;

    let inv = inv33(&ata);
    let t = inv * atb;

    Vector3::new(t[0], t[1], t[2])
}

/// IPPE decomposition: compute two rotation solutions from homography Jacobian
fn ippe_dec(v: &[f64; 2], j: &[[f64; 2]; 2]) -> (Matrix3<f64>, Matrix3<f64>, f64) {
    let t = (v[0] * v[0] + v[1] * v[1]).sqrt();

    let rv: Matrix3<f64>;
    if t < f64::EPSILON {
        rv = Matrix3::identity();
    } else {
        let s = (v[0] * v[0] + v[1] * v[1] + 1.0).sqrt();
        let costh = 1.0 / s;
        let sinth = (1.0 - 1.0 / (s * s)).sqrt();

        let k02 = v[0] / t;
        let k12 = v[1] / t;
        let k20 = -v[0] / t;
        let k21 = -v[1] / t;

        let kcrs = Matrix3::new(0.0, 0.0, k02, 0.0, 0.0, k12, k20, k21, 0.0);
        rv = Matrix3::identity() + sinth * kcrs + (1.0 - costh) * (kcrs * kcrs);
    }

    let b00 = rv[(0, 0)] - v[0] * rv[(2, 0)];
    let b01 = rv[(0, 1)] - v[0] * rv[(2, 1)];
    let b10 = rv[(1, 0)] - v[1] * rv[(2, 0)];
    let b11 = rv[(1, 1)] - v[1] * rv[(2, 1)];

    let dt = b00 * b11 - b01 * b10;
    let bi00 = b11 / dt;
    let bi01 = -b01 / dt;
    let bi10 = -b10 / dt;
    let bi11 = b00 / dt;

    let a00 = bi00 * j[0][0] + bi01 * j[1][0];
    let a01 = bi00 * j[0][1] + bi01 * j[1][1];
    let a10 = bi10 * j[0][0] + bi11 * j[1][0];
    let a11 = bi10 * j[0][1] + bi11 * j[1][1];

    let aat00 = a00 * a00 + a01 * a01;
    let aat11 = a10 * a10 + a11 * a11;
    let aat01 = a00 * a10 + a01 * a11;
    let gamma = (0.5 * (aat00 + aat11 + ((aat00 - aat11).powi(2) + 4.0 * aat01 * aat01).sqrt())).sqrt();

    if gamma.abs() < 1e-15 {
        return (Matrix3::identity(), Matrix3::identity(), 0.0);
    }

    let r00 = a00 / gamma;
    let r01 = a01 / gamma;
    let r10 = a10 / gamma;
    let r11 = a11 / gamma;

    let h00 = 1.0 - (r00 * r00 + r10 * r10);
    let h01 = -(r00 * r01 + r10 * r11);
    let h11 = 1.0 - (r01 * r01 + r11 * r11);

    let b0 = h00.max(0.0).sqrt();
    let mut b1 = h11.max(0.0).sqrt();
    if h01 < 0.0 {
        b1 = -b1;
    }

    let v1 = Vector3::new(r00, r10, b0);
    let v2 = Vector3::new(r01, r11, b1);
    let d = v1.cross(&v2);

    let c0 = d[0];
    let c1 = d[1];
    let a_val = d[2];

    let m1 = Matrix3::new(r00, r01, c0, r10, r11, c1, b0, b1, a_val);
    let m2 = Matrix3::new(r00, r01, -c0, r10, r11, -c1, -b0, -b1, a_val);

    let r1 = rv * m1;
    let r2 = rv * m2;

    (r1, r2, gamma)
}

/// Inverse of a 3x3 matrix (Cramer's rule)
fn inv33(a: &DMatrix<f64>) -> DMatrix<f64> {
    let a11 = a[(0, 0)]; let a12 = a[(0, 1)]; let a13 = a[(0, 2)];
    let a21 = a[(1, 0)]; let a22 = a[(1, 1)]; let a23 = a[(1, 2)];
    let a31 = a[(2, 0)]; let a32 = a[(2, 1)]; let a33 = a[(2, 2)];

    let det = a11 * a22 * a33 - a11 * a23 * a32 - a12 * a21 * a33
        + a12 * a23 * a31 + a13 * a21 * a32 - a13 * a22 * a31;

    let mut inv = DMatrix::zeros(3, 3);
    inv[(0, 0)] = (a22 * a33 - a23 * a32) / det;
    inv[(0, 1)] = (a13 * a32 - a12 * a33) / det;
    inv[(0, 2)] = (a12 * a23 - a13 * a22) / det;
    inv[(1, 0)] = (a23 * a31 - a21 * a33) / det;
    inv[(1, 1)] = (a11 * a33 - a13 * a31) / det;
    inv[(1, 2)] = (a13 * a21 - a11 * a23) / det;
    inv[(2, 0)] = (a21 * a32 - a22 * a31) / det;
    inv[(2, 1)] = (a12 * a31 - a11 * a32) / det;
    inv[(2, 2)] = (a11 * a22 - a12 * a21) / det;

    inv
}

/// DLT homography estimation
fn homography2d(x1: &DMatrix<f64>, x2: &DMatrix<f64>) -> Option<Matrix3<f64>> {
    let n = x1.ncols();
    if n < 4 || x2.ncols() != n {
        return None;
    }

    let (x1n, t1) = normalise2dpts(x1);
    let (x2n, t2) = normalise2dpts(x2);

    let mut a = DMatrix::zeros(3 * n, 9);
    for i in 0..n {
        let xi = [x1n[(0, i)], x1n[(1, i)], x1n[(2, i)]];
        let x = x2n[(0, i)];
        let y = x2n[(1, i)];
        let w = x2n[(2, i)];

        a[(3 * i, 3)] = -w * xi[0];
        a[(3 * i, 4)] = -w * xi[1];
        a[(3 * i, 5)] = -w * xi[2];
        a[(3 * i, 6)] = y * xi[0];
        a[(3 * i, 7)] = y * xi[1];
        a[(3 * i, 8)] = y * xi[2];

        a[(3 * i + 1, 0)] = w * xi[0];
        a[(3 * i + 1, 1)] = w * xi[1];
        a[(3 * i + 1, 2)] = w * xi[2];
        a[(3 * i + 1, 6)] = -x * xi[0];
        a[(3 * i + 1, 7)] = -x * xi[1];
        a[(3 * i + 1, 8)] = -x * xi[2];

        a[(3 * i + 2, 0)] = -y * xi[0];
        a[(3 * i + 2, 1)] = -y * xi[1];
        a[(3 * i + 2, 2)] = -y * xi[2];
        a[(3 * i + 2, 3)] = x * xi[0];
        a[(3 * i + 2, 4)] = x * xi[1];
        a[(3 * i + 2, 5)] = x * xi[2];
    }

    let svd = a.svd(false, true);
    let v_t = svd.v_t?;

    let h_vec = v_t.row(8);
    let h_norm = Matrix3::new(
        h_vec[0], h_vec[1], h_vec[2],
        h_vec[3], h_vec[4], h_vec[5],
        h_vec[6], h_vec[7], h_vec[8],
    );

    let t2_inv = t2.try_inverse()?;
    let h = t2_inv * h_norm * t1;

    Some(h)
}

/// Normalize 2D homogeneous points
fn normalise2dpts(pts: &DMatrix<f64>) -> (DMatrix<f64>, Matrix3<f64>) {
    let n = pts.ncols();
    let mut pts_out = pts.clone();

    for j in 0..n {
        let w = pts_out[(2, j)];
        if w.abs() > f64::EPSILON {
            pts_out[(0, j)] /= w;
            pts_out[(1, j)] /= w;
            pts_out[(2, j)] = 1.0;
        }
    }

    let cx: f64 = (0..n).map(|j| pts_out[(0, j)]).sum::<f64>() / n as f64;
    let cy: f64 = (0..n).map(|j| pts_out[(1, j)]).sum::<f64>() / n as f64;

    let mean_dist: f64 = (0..n)
        .map(|j| {
            let dx = pts_out[(0, j)] - cx;
            let dy = pts_out[(1, j)] - cy;
            (dx * dx + dy * dy).sqrt()
        })
        .sum::<f64>()
        / n as f64;

    let scale = if mean_dist > f64::EPSILON {
        2.0_f64.sqrt() / mean_dist
    } else {
        1.0
    };

    let t = Matrix3::new(
        scale, 0.0, -scale * cx,
        0.0, scale, -scale * cy,
        0.0, 0.0, 1.0,
    );

    let mut newpts = DMatrix::zeros(3, n);
    for j in 0..n {
        for i in 0..3 {
            newpts[(i, j)] = t[(i, 0)] * pts_out[(0, j)]
                + t[(i, 1)] * pts_out[(1, j)]
                + t[(i, 2)] * pts_out[(2, j)];
        }
    }

    (newpts, t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ippe_2d_with_known_geometry() {
        let model_pts = [
            [-0.015, 0.015],
            [-0.015, -0.015],
            [0.015, 0.015],
            [0.015, -0.015],
        ];

        let t_true = Vector3::new(0.0, 0.0, 2.0);

        let mut img_pts = [[0.0; 2]; 4];
        for (i, mp) in model_pts.iter().enumerate() {
            let px = mp[0] + t_true[0];
            let py = mp[1] + t_true[1];
            let pz = 0.0 + t_true[2];
            img_pts[i] = [px / pz, py / pz];
        }

        let result = mat_run_2d(&model_pts, &img_pts);
        assert!(result.is_some(), "IPPE should return solutions");

        let solutions = result.unwrap();
        assert!(solutions[0].reproj_err < 1e-6, "Reproj error: {}", solutions[0].reproj_err);
        assert!((solutions[0].t - t_true).norm() < 1e-6, "t: {:?}", solutions[0].t);
    }

    #[test]
    fn test_ippe_3d_with_known_geometry() {
        // 3D points (same as 2D but with explicit z=0)
        let model_pts = [
            [-0.015, 0.015, 0.0],
            [-0.015, -0.015, 0.0],
            [0.015, 0.015, 0.0],
            [0.015, -0.015, 0.0],
        ];

        let t_true = Vector3::new(0.0, 0.0, 2.0);

        let mut img_pts = [[0.0; 2]; 4];
        for (i, mp) in model_pts.iter().enumerate() {
            let px = mp[0] + t_true[0];
            let py = mp[1] + t_true[1];
            let pz = mp[2] + t_true[2];
            img_pts[i] = [px / pz, py / pz];
        }

        let result = mat_run_3d(&model_pts, &img_pts);
        assert!(result.is_some(), "IPPE 3D should return solutions");

        let solutions = result.unwrap();
        assert!(solutions[0].reproj_err < 1e-4, "Reproj error: {}", solutions[0].reproj_err);
    }
}
