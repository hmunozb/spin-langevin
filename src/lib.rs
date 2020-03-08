use itertools::Itertools;
use nalgebra::Vector3;

use ndarray::{Array2, ArrayView1, ArrayView2, ArrayView3, ArrayViewMut1, Axis};
use ndarray::parallel::prelude::*;
use num_traits::Zero;
use rand::Rng;
use rayon::prelude::*;
use simd_phys::r3::{Matrix3d4xf64, Vector3d4xf64};
use simd_phys::r3::cross_exponential_vector3d;
use simd_phys::vf64::Aligned4xf64;

#[derive(Copy, Clone)]
pub enum StepResult{
    Accept(f64),
    Reject(f64)
}

pub fn xyz_to_array_chunks(arr: ArrayView2<f64>,
                           mut chunk_array: ArrayViewMut1<Vector3d4xf64>) {
    let shape = arr.shape();
    if shape[1] != 3{
        panic!("xyz_to_array_chunks: 3 spatial dimensions required.");
    }
    let n = shape[0];
    let n_ch = (n-1)/4 + 1;
    if chunk_array.shape()[0] != n_ch{
        panic!("xyz_to_array_chunks: mismatching chunk size")
    }

    for (xyz_chunk, mut chunk_4xf64) in ArrayView2::axis_chunks_iter(&arr,Axis(0), 4)
        .zip(chunk_array.iter_mut())
    {
        //xyz_chunk is a 4 x 3D view, while chunk_4xf64 is a a single 3D x 4xf64 array

        //transpose is now 3D x 4
        let xyz_chunk_t = xyz_chunk.t();
        for (x1, x2) in xyz_chunk_t.genrows().into_iter().zip(chunk_4xf64.iter_mut()){
            for (&x1i, x2i) in x1.iter().zip(x2.dat.iter_mut()){
                *x2i = x1i;
            }
        }
    }

}

/// Evaluates v in the dynamical spin-langevin equation
///  dm/dt = v \cross m
/// where
///     v =  h - \chi (h \cross m) )
/// Specifically, this function updates the hamiltonian field by adding the dissipative term
///     h -= \chi (h\cross m)
///
/// h: Hamiltonian local fields for each spin
/// m: the 3D rotor spin
///
/// The arrays are passed as arrays of 3_D x 4_vf64 chunks
fn sl_add_dissipative(
    h_array: &mut ArrayViewMut1<Vector3d4xf64>,
    m_array: & ArrayView1<Vector3d4xf64>,
    chi: f64
){
    let chi = Aligned4xf64::from(chi);
    for (m,h) in m_array.iter().zip(h_array.iter_mut()){
        let dh = h.cross(m);
        *h -= dh * chi;
    }
}

fn sl_dissipative(
    h_array: & ArrayViewMut1<Vector3d4xf64>,
    v_array: &mut ArrayViewMut1<Vector3d4xf64>,
    m_array: & ArrayView1<Vector3d4xf64>,
    chi: f64
){
    let chi = Aligned4xf64::from(chi);
    for ((m,h), v) in m_array.iter().zip(h_array.iter()).zip(v_array.iter_mut()){
        let dh = h.cross(m);
        *v = -dh * chi;
    }
}

pub struct SpinLangevinWorkpad{
    pub m0: Array2<Vector3d4xf64>,
    pub h0: Array2<Vector3d4xf64>,
    pub h1: Array2<Vector3d4xf64>,
    pub h2: Array2<Vector3d4xf64>,
    pub m1: Array2<Vector3d4xf64>,
    pub omega1: Array2<Vector3d4xf64>,
    pub omega2: Array2<Vector3d4xf64>,
    pub chi1: Array2<Vector3d4xf64>,
    pub chi2: Array2<Vector3d4xf64>

}

impl SpinLangevinWorkpad{
    pub fn from_shape(s0: usize, s1: usize) -> Self{
        let sh = (s0, s1);
        Self{
            m0: Array2::from_elem(sh,Zero::zero()),
            h0: Array2::from_elem(sh, Zero::zero()), h1: Array2::from_elem(sh, Zero::zero()), h2:  Array2::from_elem(sh, Zero::zero()),
            m1:  Array2::from_elem(sh,Zero::zero()), omega1:  Array2::from_elem(sh,Zero::zero()), omega2:  Array2::from_elem(sh,Zero::zero()),
            chi1: Array2::from_elem(sh,Zero::zero()), chi2: Array2::from_elem(sh,Zero::zero())
        }
    }

    pub fn shape(&self) -> (usize, usize){
        let sh = self.m0.shape();

        (sh[0], sh[1])
    }
}

/// Peform a step of the Spin-Langevin stochastic differential equation (Stratonovich form)
/// using a 2nd order nonlinear Magnus propagator
///
///      \dd M   =  ( H(M) + \eta H(M) \cross M ) \cross M  \dd t + \sqrt(b) \dd xi(t) \cross M
///
/// Parameters:
/// work: SpinLangevinWorkpad, arrays of instances x spins x (3D x 4) SIMD packets
///         i.e. a total of (4*instances) x spins  3D Euclidean vectors
/// haml_update: Function pdate the local fields due to the spins at time t. Should read/modify
///             an ArrayView1<Vector3d4xf64>, where the array dimension is over spin indices
///     NOTE: haml_update must write to all three cartesian components of each local field, even if
///         a component is zero. The fields are not reset in any way before each iteration.
/// eta : Dissipation strength
/// b: stochastic noise strength  (Should be proportional to $ K_b T \eta$ for a temperature T. See note)
/// rng: an RNG engine
/// rand_xi_f: Noise increment process. (Typically normalized Gaussian noise)
///
/// NOTE ON SEMICLASSICAL PHYSICS
///     The Spin-Langevin equation is obtained as the semiclassical limit of N unentangled
///     spin S particles interacting according to a quantum Hamiltonian on the spin states. It is
///     numerically optimized for N-body SO(3) dynamics due to the Lie algebra isomorphism
///         [ S_x,  S_y ] = i S_z         <--->          e_x \cross e_y = e_z   (and cyclic perms.)
///     where S_i are the angular momentum operators of the spins and e_i are 3D Euclidean unit vectors.
///
///     In particular, for the semiclassical limit of a N-spin-1/2 Hamiltonian in terms of Pauli matrices,
///     as $S_i = \frac{1}{2} \sigma_i  (\hbar \equiv 1)$,  each K-body interaction term should be
///     rescaled by 2^K. Additionally, the single-qubit coupling $\eta$ of the open system dynamics
///     should be rescaled by 2 in the Spin-Langevin equation. Failing to rescale will result
///     in (likely incorrect) dynamics over an incorrect time scale.
///
///     Nuclear/Particle physics applications should similarly rescale by the gyromagnetic ratio
///     where appropriate so that the Hamiltonian is in terms of S_i operators rather than
///     magnetic moments.
///
/// NOTE ON SDE FORM:
///     The SDE stepping method used here is based on the Stratonovich form.
///     However, the Ito and Stratonovich forms of the Spin-Langevin equations are the same
///     to accuracy $ O( \eta * k_b T * \delta_t )$. The Stratonovich form is preferred as it corresponds
///     to the physical limit where the correlation time of the noise source goes to zero.
///
///
/// Useful References:
/// 1.  Jayannavar, A. M. Brownian motion of spins; generalized spin Langevin equation.
///     Z. Physik B - Condensed Matter 82, 153–156 (1991).
/// 2.  Albash, T. & Lidar, D. A. Demonstration of a Scaling Advantage for a Quantum Annealer over
///     Simulated Annealing. Phys. Rev. X 8, 031016 (2018).
///
pub fn spin_langevin_step<Fh, R, Fr>(
    m0: &Array2<Vector3d4xf64>, mf: &mut Array2<Vector3d4xf64>,
    t0: f64, delta_t : f64,
    work :&mut SpinLangevinWorkpad,
    eta: f64, b: f64,
    haml_fn: Fh,
    rng: &mut R,
    rand_xi_f: Fr,
) -> StepResult
    where Fh: Fn(f64, &ArrayView1<Vector3d4xf64>, &mut ArrayViewMut1<Vector3d4xf64>) + Sync,
          R: Rng + ?Sized,
          Fr: Fn(&mut R) -> Vector3d4xf64
{
    let h_shape = work.shape();
    let t1 = t0 + delta_t/2.0;
    let t2 = t0 + delta_t;
    let delta_t = Aligned4xf64::from(delta_t);

    assert_eq!(m0.raw_dim(), work.h0.raw_dim());
    assert_eq!(mf.raw_dim(), m0.raw_dim());
    assert!(b >= 0.0, "Stochastic strength must be non-negative");

    let b_sqrt = Aligned4xf64::from(b.sqrt());
    // Populate random noise arrays
    let noise_1 = &mut work.chi1;
    let noise_2 = &mut work.chi2;
    for (chi1, chi2) in itertools::zip(noise_1.iter_mut(), noise_2.iter_mut()){
        *chi1 = rand_xi_f(rng) * b_sqrt;
        *chi2 = rand_xi_f(rng) * b_sqrt;
    }
    // Hamiltonian field update
    let h_update = |t: f64, h: &mut Array2<Vector3d4xf64>, m: & Array2<Vector3d4xf64> |{
        h.axis_iter_mut(Axis(0)).into_par_iter().zip(m.axis_iter(Axis(0)).into_par_iter())
            .for_each(|(mut h_row, m_row)|{
                haml_fn(t, &m_row, &mut h_row);
                sl_add_dissipative(&mut h_row, & m_row, eta);
            });
    };
    let avg_field = |m: & Array2<Vector3d4xf64>| -> f64{
        let m_sum : f64 = m.iter().map(|v: &Vector3d4xf64|
            (v[0]*v[0] + v[1]*v[1] + v[2]*v[2]).map(f64::sqrt).mean_reduce())
            .sum() ;
        m_sum / (m.len() as f64)

    };
    // Spin propagation update
    let m_update = |omega: &Array2<Vector3d4xf64>, spins_t0: &Array2<Vector3d4xf64>,
                    spins_tf: &mut Array2<Vector3d4xf64>|
        {

            ndarray::Zip::from(omega).and(spins_t0).and(spins_tf)
                .into_par_iter()
                //.with_min_len(4*h_shape.0)
                .for_each(
                    |(om, m0, mf)|{
                        let mut phi : Matrix3d4xf64 = Zero::zero();
                        cross_exponential_vector3d(om, &mut phi);
                        phi.mul_to(m0, mf);
                    }
                );
        };

    //let m0 = &work.m0;
    let haml_10 = &mut work.h0;
    let haml_11 = &mut work.h1;
    let haml_12 = &mut work.h2;

    // The nonlinear Magnus Expansion to 2nd order is as follows:
    //
    // STAGE 1
    // m_10  =  m_0,
    // H_{10} = H(t_0, m0),     H_{11} = H(t_1, m0)     H_{12} = H(t_2, m0)
    // \Omega_{11}  =  (\delta_t / 4) ( H_{10}  + H_{11} ) + b \sqrt{\delta_t/2} \chi_1
    // \Omega_{12} = (\delta_t / 6) (H_{10} + 4 H_{11} + H_{12} + b \sqrt{\delta_t/2} (\chi_1 + \chi_2)
    //
    // STAGE 2
    // m_{20} = m0,    m_{21} = \exp{\Omega_{11}} m_0,    m_{22} = \exp{\Omega_{12}} m_0
    // H_{20} =  H_{10},    H_{21} =H(t_1, m_{21}),     H_{22} = H(t_2, m_{22}
    // \Omega_2 = (\delta_t / 6) (H_{20} + 4 H_{21} + H_{22} + b \sqrt{\delta_t/2} (\chi_1 + \chi_2)
    //
    // Final propagation:
    // m[\delta_t] :=  \exp{\Omega_{22}} m_0

    // Stage 1 Computation
    h_update(t0, haml_10, m0);
    h_update(t1, haml_11, m0);
    h_update(t2, haml_12, m0);


    // Generator updates
    let omega_11 = &mut work.omega1;
    ndarray::Zip::from(haml_10.view()).and(haml_11.view()).and(omega_11.view_mut())
        .and(noise_1.view())
        .into_par_iter()
        .for_each(|(h0, h1, o1, chi1)|{
            *o1 = (h0 + h1) * Aligned4xf64::from(delta_t / 4.0)
                + chi1 * (delta_t / 2.0).map(f64::sqrt);
        });

    let omega_12 = &mut work.omega2;
    ndarray::Zip::from(haml_10.view()).and(haml_11.view()).and(haml_12.view()).and(omega_12.view_mut())
        .and(noise_1.view()).and(noise_2.view()).into_par_iter()
        .for_each(|(h0, h1, h2, o2, chi1, chi2)|{
            *o2 = (h0 + h1 * Aligned4xf64::from(4.0) + h2) * (delta_t / 6.0)
                + (chi1 + chi2) * (delta_t/2.0).map(f64::sqrt);
        });

    // Check that the norm of the first stage is not too large
    // Otherwise, dissipative term can cause numerical instability
    let mean_o12 = avg_field(&*omega_12);
    if mean_o12 >= 1.0 {
        return StepResult::Reject(mean_o12);
    }

    let spins_t0 = m0;
    let spins_t = mf;
    let haml_20 = haml_10;
    let haml_21 = haml_11;
    let haml_22 = haml_12;

    // Stage 2 computation

    // Evaluate m21 then update H21
    m_update(&*omega_11, spins_t0, spins_t);
    h_update(t1, haml_21, &*spins_t);

    // Evaluate m22 then update H22
    m_update(&*omega_12, spins_t0, spins_t);
    h_update(t2, haml_22, &*spins_t);


    // Finally evaluate \Omega_2
    let omega2 = &mut work.omega2;

    ndarray::Zip::from(haml_20.view()).and(haml_21.view()).and(haml_22.view()).and(omega2.view_mut())
        .and(noise_1.view()).and(noise_2.view()).into_par_iter()
        .for_each(|(h0, h1, h2, o2, chi1, chi2)|{
            *o2 = (h0 + h1 * Aligned4xf64::from(4.0) + h2) * (delta_t / 6.0)
                + (chi1 + chi2) * (delta_t/2.0).map(f64::sqrt);
        });

    // Propagate m[0] to m[\delta_t]
    m_update(&*omega2, spins_t0, spins_t);

    let mean_o22 = avg_field(&*omega2);
    return StepResult::Accept(mean_o22);

}

#[cfg(test)]
mod tests{
    use ndarray::{Array1, Array2};
    use num_traits::Zero;

    use super::{sl_add_dissipative, spin_langevin_step, Vector3d4xf64,
                                xyz_to_array_chunks};
    use simd_phys::vf64::Aligned4xf64;

    #[test]
    fn test_spin_langevin_dmdt(){
        let haml_arr = Array2::from_shape_vec((4,3),
                                              vec![ 1.0, 0.0, 0.0,
                                                    0.0, 1.0, 0.0,
                                                    0.5, 0.5, 0.0,
                                                    0.5, -0.5, 0.0]).unwrap();
        let mut haml = Array1::from_elem((1,), Zero::zero());

        xyz_to_array_chunks(haml_arr.view(), haml.view_mut());
        let spins_arr = Array2::from_shape_vec((4, 3),
                                               vec![0.0, 0.0, 1.0,    0.0, 0.0, 1.0,    0.0, 0.0, 1.0,    0.0, 0.0, 1.0]
        ).unwrap();
        let mut spins = Array1::from_elem((1,), Zero::zero());
        xyz_to_array_chunks(spins_arr.view(), spins.view_mut());

        //let mut dm = Array1::from_elem((1,), ZERO_SPIN_ARRAY_3D);

        sl_add_dissipative(&mut haml.view_mut(), & spins.view(), 0.1);
    }

}