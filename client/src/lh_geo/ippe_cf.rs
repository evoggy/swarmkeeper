/// CF-specific IPPE wrapper
///
/// Converts between Crazyflie coordinate system and IPPE (OpenCV) coordinate system.
/// Ported from ippe_cf.py

use nalgebra::{Matrix3, Vector3};

use super::ippe;

/// Rotation matrix: IPPE (OpenCV) → CF coordinate system
const R_IPPE_TO_CF: Matrix3<f64> = Matrix3::new(
    0.0, 0.0, 1.0,
    -1.0, 0.0, 0.0,
    0.0, -1.0, 0.0,
);

/// Rotation matrix: CF → IPPE (OpenCV) coordinate system (= transpose of above)
fn r_cf_to_ippe() -> Matrix3<f64> {
    R_IPPE_TO_CF.transpose()
}

#[derive(Debug, Clone)]
pub struct IppeCfSolution {
    pub r: Matrix3<f64>,
    pub t: Vector3<f64>,
    pub reproj_err: f64,
}

pub struct IppeCf;

impl IppeCf {
    /// Solve for pose given CF-convention sensor positions and projection points.
    ///
    /// sensor_positions: 4x3 array of sensor positions in CF world coordinates (z=0)
    /// projections: 4x2 array of projection points (y, z in CF convention)
    ///
    /// Returns two solutions sorted by reprojection error (best first).
    pub fn solve(
        sensor_positions: &[[f64; 3]; 4],
        projections: &[[f64; 2]; 4],
    ) -> Option<[IppeCfSolution; 2]> {
        let r_to_ippe = r_cf_to_ippe();

        // Convert sensor positions from CF to IPPE coordinate system (3D)
        let mut u_ippe = [[0.0; 3]; 4];
        for i in 0..4 {
            let p = Vector3::new(
                sensor_positions[i][0],
                sensor_positions[i][1],
                sensor_positions[i][2],
            );
            let p_ippe = r_to_ippe * p;
            u_ippe[i] = [p_ippe[0], p_ippe[1], p_ippe[2]];
        }

        // Convert projection points from CF to IPPE: negate both components
        let mut q_ippe = [[0.0; 2]; 4];
        for i in 0..4 {
            q_ippe[i] = [-projections[i][0], -projections[i][1]];
        }

        let solutions = ippe::mat_run_3d(&u_ippe, &q_ippe)?;

        // Convert solutions back to CF coordinate system
        let sol1 = ippe_to_cf(&solutions[0]);
        let sol2 = ippe_to_cf(&solutions[1]);

        Some([sol1, sol2])
    }
}

fn ippe_to_cf(sol: &ippe::IppeSolution) -> IppeCfSolution {
    let r_to_cf = R_IPPE_TO_CF;
    let r_to_ippe = r_cf_to_ippe();

    IppeCfSolution {
        r: r_to_cf * sol.r * r_to_ippe,
        t: r_to_cf * sol.t,
        reproj_err: sol.reproj_err,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lh_geo::types::LhDeck4SensorPositions;

    #[test]
    fn test_ippe_cf_basic() {
        use crate::lh_geo::bs_vector::LighthouseBsVector;

        let sensor_pos = LhDeck4SensorPositions::positions();

        // Place a BS at (2, 0, 1) in world frame, looking at origin
        // BS reference frame: x-axis points forward (toward what BS sees)
        // In the BS local frame, the CF sensors project onto the x=1 plane
        let bs_pos = Vector3::new(2.0, 0.0, 1.0);

        // BS rotation: x-axis of BS points from BS toward world origin
        // direction = (0,0,0) - (2,0,1) = (-2,0,-1), normalized
        let dir = Vector3::new(-2.0, 0.0, -1.0).normalize();
        // Build a rotation where first column = dir, others orthogonal
        let up = Vector3::new(0.0, 1.0, 0.0);
        let right = dir.cross(&up).normalize();
        let new_up = right.cross(&dir).normalize();
        let r_bs = Matrix3::from_columns(&[dir, right, new_up]);

        // For each sensor, compute what angle the BS would measure
        let mut projections = [[0.0; 2]; 4];
        for (i, sp) in sensor_pos.iter().enumerate() {
            let p_world = Vector3::new(sp[0], sp[1], sp[2]);
            // Transform to BS local frame
            let p_local = r_bs.transpose() * (p_world - bs_pos);
            // Projection onto x=1 plane gives (y/x, z/x)
            // In CF convention for IPPE: these become the projection points
            let bsv = LighthouseBsVector::from_cart(&[p_local[0], p_local[1], p_local[2]]);
            projections[i] = bsv.projection();
        }

        let result = IppeCf::solve(&sensor_pos, &projections);
        assert!(result.is_some(), "Should find solutions");
        let solutions = result.unwrap();
        assert!(
            solutions[0].reproj_err < 0.1,
            "Best solution reproj error should be small, got {}",
            solutions[0].reproj_err
        );
    }
}
