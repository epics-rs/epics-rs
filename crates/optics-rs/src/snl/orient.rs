//! 4-circle diffractometer orientation matrix state machine.
//!
//! Pure Rust port of `orient_st.st` — drives TTH, TH, CHI, PHI motors
//! based on HKL requests using orientation-matrix calculations from
//! [`crate::math::orient`].

use epics_base_rs::server::database::PvDatabase;

use crate::db_access::{DbChannel, DbMultiMonitor, alloc_origin};
use crate::math::matrix3::{IDENTITY, Mat3, Vec3};
use crate::math::orient::{
    Constraint, angles_to_hkl, calc_a0, calc_omtx, check_omtx, hkl_to_angles,
};

/// Energy-wavelength conversion constant: E(keV) = HC / lambda(Angstroms).
const HC: f64 = 12.3984244;

/// Threshold below which a value is treated as "effectively zero".
const SMALL: f64 = 1.0e-9;

/// Calculation state for A0 or OMTX.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalcState {
    Needed,
    Succeeded,
    Failed,
}

/// Diffractometer state machine states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrientState {
    Init,
    Idle,
    NewHKL,
    NewAngles,
    NewMotors,
    PutAll,
    WaitingForMotors,
}

/// Lattice parameters for A0 calculation.
#[derive(Debug, Clone)]
pub struct LatticeParams {
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub alpha: f64,
    pub beta: f64,
    pub gamma: f64,
}

impl Default for LatticeParams {
    fn default() -> Self {
        Self {
            a: 5.431,
            b: 5.431,
            c: 5.431,
            alpha: 90.0,
            beta: 90.0,
            gamma: 90.0,
        }
    }
}

/// A reference reflection (HKL + angles).
#[derive(Debug, Clone, Default)]
pub struct Reflection {
    pub h: f64,
    pub k: f64,
    pub l: f64,
    pub tth: f64,
    pub th: f64,
    pub chi: f64,
    pub phi: f64,
}

impl Reflection {
    pub fn hkl(&self) -> Vec3 {
        [self.h, self.k, self.l]
    }

    pub fn angles(&self) -> [f64; 4] {
        [self.tth, self.th, self.chi, self.phi]
    }
}

/// Configuration for the orient state machine (PV names from macros).
#[derive(Debug, Clone)]
pub struct OrientConfig {
    pub prefix: String,
    pub motor_prefix: String,
    pub motor_tth: String,
    pub motor_th: String,
    pub motor_chi: String,
    pub motor_phi: String,
}

impl OrientConfig {
    pub fn new(p: &str, pm: &str, m_tth: &str, m_th: &str, m_chi: &str, m_phi: &str) -> Self {
        Self {
            prefix: p.to_string(),
            motor_prefix: pm.to_string(),
            motor_tth: format!("{pm}{m_tth}"),
            motor_th: format!("{pm}{m_th}"),
            motor_chi: format!("{pm}{m_chi}"),
            motor_phi: format!("{pm}{m_phi}"),
        }
    }

    pub fn motor_tth_rbv(&self) -> String {
        let base = self.motor_tth.split('.').next().unwrap_or(&self.motor_tth);
        format!("{base}.RBV")
    }

    pub fn motor_th_rbv(&self) -> String {
        let base = self.motor_th.split('.').next().unwrap_or(&self.motor_th);
        format!("{base}.RBV")
    }

    pub fn motor_chi_rbv(&self) -> String {
        let base = self.motor_chi.split('.').next().unwrap_or(&self.motor_chi);
        format!("{base}.RBV")
    }

    pub fn motor_phi_rbv(&self) -> String {
        let base = self.motor_phi.split('.').next().unwrap_or(&self.motor_phi);
        format!("{base}.RBV")
    }
}

/// 4-circle diffractometer state machine controller.
///
/// Pure logic that computes angles from HKL and vice versa, managing the
/// A0 and OMTX matrices, and tracking which motors need to be driven.
#[derive(Debug, Clone)]
pub struct OrientController {
    pub state: OrientState,

    // Matrices
    pub a0: Mat3,
    pub a0_inv: Mat3,
    pub omtx: Mat3,
    pub omtx_inv: Mat3,
    pub a0_state: CalcState,
    pub omtx_state: CalcState,

    // Crystal parameters
    pub lattice: LatticeParams,
    pub energy: f64,
    pub lambda: f64,

    // Reflections
    pub ref1: Reflection,
    pub ref2: Reflection,

    // HKL
    pub h: f64,
    pub k: f64,
    pub l: f64,
    pub h_rbv: f64,
    pub k_rbv: f64,
    pub l_rbv: f64,

    // Trial angles (degrees)
    pub tth: f64,
    pub th: f64,
    pub chi: f64,
    pub phi: f64,

    // Actual motor positions (degrees)
    pub mot_tth: f64,
    pub mot_th: f64,
    pub mot_chi: f64,
    pub mot_phi: f64,

    // Constraint mode
    pub mode: Constraint,

    // Control flags
    pub mot_put_auto: bool,
    pub mot_get_auto: bool,
    pub busy: bool,
    pub waiting_for_motors: bool,

    // Error angle for OMTX check
    pub err_angle: f64,
    pub err_angle_thresh: f64,

    // Output flags: what needs to be written
    pub new_hkl: bool,
    pub new_angles: bool,
    pub new_motors: bool,
}

impl Default for OrientController {
    fn default() -> Self {
        Self {
            state: OrientState::Init,
            a0: IDENTITY,
            a0_inv: IDENTITY,
            omtx: IDENTITY,
            omtx_inv: IDENTITY,
            a0_state: CalcState::Needed,
            omtx_state: CalcState::Needed,
            lattice: LatticeParams::default(),
            energy: 0.0,
            lambda: 0.0,
            ref1: Reflection::default(),
            ref2: Reflection::default(),
            h: 0.0,
            k: 0.0,
            l: 0.0,
            h_rbv: 0.0,
            k_rbv: 0.0,
            l_rbv: 0.0,
            tth: 0.0,
            th: 0.0,
            chi: 0.0,
            phi: 0.0,
            mot_tth: 0.0,
            mot_th: 0.0,
            mot_chi: 0.0,
            mot_phi: 0.0,
            mode: Constraint::OmegaZero,
            mot_put_auto: false,
            mot_get_auto: false,
            busy: false,
            waiting_for_motors: false,
            err_angle: 0.0,
            err_angle_thresh: 1.0,
            new_hkl: false,
            new_angles: false,
            new_motors: false,
        }
    }
}

/// Events that drive the orient state machine.
#[derive(Debug, Clone)]
pub enum OrientEvent {
    /// Energy changed.
    EnergyChanged(f64),
    /// Wavelength changed.
    LambdaChanged(f64),
    /// Crystal lattice parameters changed.
    LatticeChanged(LatticeParams),
    /// HKL values changed.
    HKLChanged { h: f64, k: f64, l: f64 },
    /// Trial angles changed.
    AnglesChanged {
        tth: f64,
        th: f64,
        chi: f64,
        phi: f64,
    },
    /// Motor positions changed (external agent moved motors).
    MotorsChanged {
        tth: f64,
        th: f64,
        chi: f64,
        phi: f64,
    },
    /// Motor move completed.
    MotorsDone {
        tth: f64,
        th: f64,
        chi: f64,
        phi: f64,
    },
    /// Motor readback values changed.
    MotorRBVChanged {
        tth: f64,
        th: f64,
        chi: f64,
        phi: f64,
    },
    /// User command: calculate OMTX.
    CalcOMTX,
    /// Constraint mode changed.
    ModeChanged(Constraint),
    /// User command: "Move" (send trial angles to motors).
    MotPut,
    /// User command: "Read" (read motors to trial angles).
    MotGet,
    /// Primary reflection updated.
    Ref1Changed(Reflection),
    /// Secondary reflection updated.
    Ref2Changed(Reflection),
    /// Copy current HKL/angles to primary reflection.
    RefGet1,
    /// Copy current HKL/angles to secondary reflection.
    RefGet2,
    /// A0 matrix elements directly changed.
    A0MatrixChanged(Mat3),
    /// OMTX matrix elements directly changed.
    OMTXMatrixChanged(Mat3),
}

/// Actions the caller should take after processing an event.
#[derive(Debug, Clone, Default)]
pub struct OrientActions {
    /// Motor targets to write (tth, th, chi, phi). `None` if no motor move needed.
    pub drive_motors: Option<[f64; 4]>,
    /// Updated HKL to write back.
    pub write_hkl: Option<[f64; 3]>,
    /// Updated trial angles to write back.
    pub write_angles: Option<[f64; 4]>,
    /// Updated A0 matrix to publish.
    pub write_a0: Option<Mat3>,
    /// Updated OMTX matrix to publish.
    pub write_omtx: Option<Mat3>,
    /// Error message, if any.
    pub message: Option<String>,
    /// Whether busy flag changed.
    pub busy_changed: Option<bool>,
    /// Updated RBV HKL to write.
    pub write_hkl_rbv: Option<[f64; 3]>,
    /// Ref1 was updated from current values.
    pub write_ref1: Option<Reflection>,
    /// Ref2 was updated from current values.
    pub write_ref2: Option<Reflection>,
}

/// Synchronise energy and wavelength.
fn sync_energy_lambda(energy: &mut f64, lambda: &mut f64) {
    if *lambda > SMALL && *energy <= SMALL {
        *energy = HC / *lambda;
    } else if *energy > SMALL {
        *lambda = HC / *energy;
    }
}

impl OrientController {
    /// Recalculate the A0 matrix from current lattice params and wavelength.
    pub fn recalc_a0(&mut self) -> bool {
        sync_energy_lambda(&mut self.energy, &mut self.lambda);
        let lp = &self.lattice;
        if lp.a == 0.0
            || lp.b == 0.0
            || lp.c == 0.0
            || lp.alpha == 0.0
            || lp.beta == 0.0
            || lp.gamma == 0.0
            || self.lambda == 0.0
        {
            self.a0_state = CalcState::Failed;
            return false;
        }
        match calc_a0(lp.a, lp.b, lp.c, lp.alpha, lp.beta, lp.gamma, self.lambda) {
            Some((a0, a0_inv)) => {
                self.a0 = a0;
                self.a0_inv = a0_inv;
                self.a0_state = CalcState::Succeeded;
                true
            }
            None => {
                self.a0_state = CalcState::Failed;
                false
            }
        }
    }

    /// Recalculate the OMTX matrix from two reflections.
    pub fn recalc_omtx(&mut self) -> bool {
        let v1_hkl = self.ref1.hkl();
        let v1_angles = self.ref1.angles();
        let v2_hkl = self.ref2.hkl();
        let v2_angles = self.ref2.angles();

        // Don't attempt if either reflection HKL is zero.
        if v1_hkl == [0.0, 0.0, 0.0] || v2_hkl == [0.0, 0.0, 0.0] {
            self.omtx_state = CalcState::Failed;
            return false;
        }

        match calc_omtx(
            &v1_hkl,
            &v1_angles,
            &v2_hkl,
            &v2_angles,
            &self.a0,
            &self.a0_inv,
        ) {
            Some((o, o_inv)) => {
                self.omtx = o;
                self.omtx_inv = o_inv;
                // Verify with secondary reflection
                let err = check_omtx(&v2_hkl, &v2_angles, &self.a0, &self.a0_inv, &o_inv);
                match err {
                    Some(e) if e.abs() < self.err_angle_thresh => {
                        self.err_angle = e;
                        self.omtx_state = CalcState::Succeeded;
                        true
                    }
                    Some(e) => {
                        self.err_angle = e;
                        self.omtx_state = CalcState::Failed;
                        false
                    }
                    None => {
                        self.omtx_state = CalcState::Failed;
                        false
                    }
                }
            }
            None => {
                self.omtx_state = CalcState::Failed;
                false
            }
        }
    }

    /// Convert current HKL to trial angles using the forward transform.
    pub fn hkl_to_trial_angles(&mut self) -> bool {
        let hkl: Vec3 = [self.h, self.k, self.l];
        let mut angles = [self.tth, self.th, self.chi, self.phi];
        match hkl_to_angles(&hkl, &self.a0, &self.omtx, &mut angles, self.mode) {
            Some(()) => {
                self.tth = angles[0];
                self.th = angles[1];
                self.chi = angles[2];
                self.phi = angles[3];
                true
            }
            None => false,
        }
    }

    /// Convert current trial angles to HKL using the inverse transform.
    pub fn trial_angles_to_hkl(&mut self) -> bool {
        let angles = [self.tth, self.th, self.chi, self.phi];
        match angles_to_hkl(&angles, &self.omtx_inv, &self.a0_inv) {
            Some(hkl) => {
                self.h = hkl[0];
                self.k = hkl[1];
                self.l = hkl[2];
                true
            }
            None => false,
        }
    }

    /// Convert motor readback angles to HKL readback values.
    pub fn motor_rbv_to_hkl(&self, rbv_angles: &[f64; 4]) -> Option<[f64; 3]> {
        if self.a0_state != CalcState::Succeeded || self.omtx_state != CalcState::Succeeded {
            return None;
        }
        angles_to_hkl(rbv_angles, &self.omtx_inv, &self.a0_inv).map(|v| [v[0], v[1], v[2]])
    }

    /// Check whether trial angles differ from motor positions.
    pub fn motors_need_move(&self) -> bool {
        (self.mot_tth - self.tth).abs() > SMALL
            || (self.mot_th - self.th).abs() > SMALL
            || (self.mot_chi - self.chi).abs() > SMALL
            || (self.mot_phi - self.phi).abs() > SMALL
    }

    /// Process a single event, returning actions the caller should take.
    pub fn step(&mut self, event: OrientEvent) -> OrientActions {
        let mut actions = OrientActions::default();

        match event {
            OrientEvent::EnergyChanged(e) => {
                self.energy = e;
                self.lambda = HC / e;
                self.recalc_a0();
                actions.write_a0 = Some(self.a0);
                if self.a0_state == CalcState::Succeeded {
                    self.recalc_omtx();
                    actions.write_omtx = Some(self.omtx);
                    if self.omtx_state == CalcState::Succeeded && self.hkl_to_trial_angles() {
                        actions.write_angles = Some([self.tth, self.th, self.chi, self.phi]);
                        if self.mot_put_auto {
                            actions.drive_motors = Some([self.tth, self.th, self.chi, self.phi]);
                        }
                    }
                    actions.message = Some("Recalculated A0 and OMTX for new energy".into());
                } else {
                    actions.message = Some("A0 calc failed after energy change".into());
                }
            }

            OrientEvent::LambdaChanged(l) => {
                self.lambda = l;
                self.energy = HC / l;
                self.recalc_a0();
                actions.write_a0 = Some(self.a0);
                if self.a0_state == CalcState::Succeeded {
                    self.recalc_omtx();
                    actions.write_omtx = Some(self.omtx);
                    if self.omtx_state == CalcState::Succeeded && self.hkl_to_trial_angles() {
                        actions.write_angles = Some([self.tth, self.th, self.chi, self.phi]);
                        if self.mot_put_auto {
                            actions.drive_motors = Some([self.tth, self.th, self.chi, self.phi]);
                        }
                    }
                    actions.message = Some("Recalculated A0 and OMTX for new wavelength".into());
                } else {
                    actions.message = Some("A0 calc failed after wavelength change".into());
                }
            }

            OrientEvent::LatticeChanged(lp) => {
                self.lattice = lp;
                self.recalc_a0();
                actions.write_a0 = Some(self.a0);
                if self.a0_state == CalcState::Succeeded {
                    self.recalc_omtx();
                    actions.write_omtx = Some(self.omtx);
                    if self.omtx_state == CalcState::Succeeded && self.hkl_to_trial_angles() {
                        actions.write_angles = Some([self.tth, self.th, self.chi, self.phi]);
                        if self.mot_put_auto {
                            actions.drive_motors = Some([self.tth, self.th, self.chi, self.phi]);
                        }
                    }
                }
            }

            OrientEvent::HKLChanged { h, k, l } => {
                self.h = h;
                self.k = k;
                self.l = l;
                if self.a0_state != CalcState::Succeeded || self.omtx_state != CalcState::Succeeded
                {
                    actions.message = Some("No valid A0/OMTX matrix".into());
                    return actions;
                }
                if self.hkl_to_trial_angles() {
                    actions.write_angles = Some([self.tth, self.th, self.chi, self.phi]);
                    if self.mot_put_auto {
                        actions.drive_motors = Some([self.tth, self.th, self.chi, self.phi]);
                        self.busy = true;
                        actions.busy_changed = Some(true);
                    }
                } else {
                    actions.message = Some("HKL to angles calculation failed".into());
                }
            }

            OrientEvent::AnglesChanged { tth, th, chi, phi } => {
                self.tth = tth;
                self.th = th;
                self.chi = chi;
                self.phi = phi;
                if self.a0_state != CalcState::Succeeded || self.omtx_state != CalcState::Succeeded
                {
                    actions.message = Some("No valid A0/OMTX matrix".into());
                    return actions;
                }
                if self.trial_angles_to_hkl() {
                    actions.write_hkl = Some([self.h, self.k, self.l]);
                    if self.mot_put_auto {
                        actions.drive_motors = Some([self.tth, self.th, self.chi, self.phi]);
                        self.busy = true;
                        actions.busy_changed = Some(true);
                    }
                } else {
                    actions.message = Some("Angles to HKL calculation failed".into());
                }
            }

            OrientEvent::MotorsChanged { tth, th, chi, phi } => {
                self.mot_tth = tth;
                self.mot_th = th;
                self.mot_chi = chi;
                self.mot_phi = phi;
                if self.mot_get_auto && !self.waiting_for_motors {
                    self.tth = tth;
                    self.th = th;
                    self.chi = chi;
                    self.phi = phi;
                    actions.write_angles = Some([tth, th, chi, phi]);
                    if self.trial_angles_to_hkl() {
                        actions.write_hkl = Some([self.h, self.k, self.l]);
                    }
                }
            }

            OrientEvent::MotorsDone { tth, th, chi, phi } => {
                self.mot_tth = tth;
                self.mot_th = th;
                self.mot_chi = chi;
                self.mot_phi = phi;
                self.waiting_for_motors = false;
                if self.busy {
                    self.busy = false;
                    actions.busy_changed = Some(false);
                }
                // Back-transform to update HKL from actual motor positions
                self.tth = tth;
                self.th = th;
                self.chi = chi;
                self.phi = phi;
                actions.write_angles = Some([tth, th, chi, phi]);
                if self.trial_angles_to_hkl() {
                    actions.write_hkl = Some([self.h, self.k, self.l]);
                }
            }

            OrientEvent::MotorRBVChanged { tth, th, chi, phi } => {
                let rbv = [tth, th, chi, phi];
                if let Some(hkl) = self.motor_rbv_to_hkl(&rbv) {
                    self.h_rbv = hkl[0];
                    self.k_rbv = hkl[1];
                    self.l_rbv = hkl[2];
                    actions.write_hkl_rbv = Some(hkl);
                }
            }

            OrientEvent::CalcOMTX => {
                if self.a0_state != CalcState::Succeeded {
                    actions.message = Some("Cannot calc OMTX: no valid A0".into());
                    return actions;
                }
                if self.recalc_omtx() {
                    actions.write_omtx = Some(self.omtx);
                    actions.message = Some("Successful OMTX calc".into());
                    // Recompute angles from current HKL
                    if self.hkl_to_trial_angles() {
                        actions.write_angles = Some([self.tth, self.th, self.chi, self.phi]);
                    }
                } else {
                    actions.message = Some("Bad OMTX calc; motPut_Auto set to Manual".into());
                    self.mot_put_auto = false;
                }
            }

            OrientEvent::ModeChanged(mode) => {
                self.mode = mode;
                if self.a0_state == CalcState::Succeeded
                    && self.omtx_state == CalcState::Succeeded
                    && self.hkl_to_trial_angles()
                {
                    actions.write_angles = Some([self.tth, self.th, self.chi, self.phi]);
                    if self.mot_put_auto {
                        actions.drive_motors = Some([self.tth, self.th, self.chi, self.phi]);
                    }
                }
            }

            OrientEvent::MotPut => {
                actions.drive_motors = Some([self.tth, self.th, self.chi, self.phi]);
                self.waiting_for_motors = true;
                self.busy = true;
                actions.busy_changed = Some(true);
            }

            OrientEvent::MotGet => {
                self.tth = self.mot_tth;
                self.th = self.mot_th;
                self.chi = self.mot_chi;
                self.phi = self.mot_phi;
                actions.write_angles = Some([self.tth, self.th, self.chi, self.phi]);
                if self.trial_angles_to_hkl() {
                    actions.write_hkl = Some([self.h, self.k, self.l]);
                }
            }

            OrientEvent::Ref1Changed(r) => {
                self.ref1 = r;
                self.omtx_state = CalcState::Needed;
            }

            OrientEvent::Ref2Changed(r) => {
                self.ref2 = r;
                self.omtx_state = CalcState::Needed;
            }

            OrientEvent::RefGet1 => {
                let r = Reflection {
                    h: self.h,
                    k: self.k,
                    l: self.l,
                    tth: self.tth,
                    th: self.th,
                    chi: self.chi,
                    phi: self.phi,
                };
                self.ref1 = r.clone();
                actions.write_ref1 = Some(r);
            }

            OrientEvent::RefGet2 => {
                let r = Reflection {
                    h: self.h,
                    k: self.k,
                    l: self.l,
                    tth: self.tth,
                    th: self.th,
                    chi: self.chi,
                    phi: self.phi,
                };
                self.ref2 = r.clone();
                actions.write_ref2 = Some(r);
            }

            OrientEvent::A0MatrixChanged(m) => {
                self.a0 = m;
                if let Some(inv) = crate::math::matrix3::invert(&m) {
                    self.a0_inv = inv;
                    self.a0_state = CalcState::Succeeded;
                    actions.message = Some("User A0 matrix accepted".into());
                    // Recompute HKL from angles
                    if self.omtx_state == CalcState::Succeeded && self.hkl_to_trial_angles() {
                        actions.write_angles = Some([self.tth, self.th, self.chi, self.phi]);
                    }
                } else {
                    self.a0_state = CalcState::Failed;
                    actions.message = Some("Could not invert A0 matrix".into());
                }
            }

            OrientEvent::OMTXMatrixChanged(m) => {
                self.omtx = m;
                if let Some(inv) = crate::math::matrix3::invert(&m) {
                    self.omtx_inv = inv;
                    if self.a0_state == CalcState::Succeeded {
                        let v2_hkl = self.ref2.hkl();
                        let v2_angles = self.ref2.angles();
                        let err = check_omtx(&v2_hkl, &v2_angles, &self.a0, &self.a0_inv, &inv);
                        match err {
                            Some(e) if e.abs() < self.err_angle_thresh => {
                                self.err_angle = e;
                                self.omtx_state = CalcState::Succeeded;
                                actions.message = Some("User OMTX passes check".into());
                            }
                            Some(e) => {
                                self.err_angle = e;
                                self.omtx_state = CalcState::Failed;
                                actions.message = Some("User OMTX fails check".into());
                            }
                            None => {
                                self.omtx_state = CalcState::Failed;
                                actions.message = Some("OMTX check failed".into());
                            }
                        }
                    } else {
                        actions.message = Some("Cannot check OMTX: no valid A0".into());
                        self.omtx_state = CalcState::Failed;
                    }
                } else {
                    self.omtx_state = CalcState::Failed;
                    actions.message = Some("Could not invert OMTX matrix".into());
                }
            }
        }

        actions
    }
}

/// Async entry point — runs the orient state machine against live PVs.
///
/// This monitors HKL, angle, energy, and motor PVs, dispatching events
/// to [`OrientController::step`] and applying the resulting actions.
pub async fn run(
    config: OrientConfig,
    db: PvDatabase,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use tokio::time::{Duration, sleep};

    tokio::time::sleep(Duration::from_secs(3)).await;
    println!("orient: starting for prefix={}", config.prefix);

    let my_origin = alloc_origin();

    let p = &config.prefix;

    // Connect motor channels
    let ch_mot_tth = DbChannel::new(&db, &config.motor_tth);
    let ch_mot_th = DbChannel::new(&db, &config.motor_th);
    let ch_mot_chi = DbChannel::new(&db, &config.motor_chi);
    let ch_mot_phi = DbChannel::new(&db, &config.motor_phi);

    // Motor readback channels
    let ch_mot_tth_rbv = DbChannel::new(&db, &config.motor_tth_rbv());
    let ch_mot_th_rbv = DbChannel::new(&db, &config.motor_th_rbv());
    let ch_mot_chi_rbv = DbChannel::new(&db, &config.motor_chi_rbv());
    let ch_mot_phi_rbv = DbChannel::new(&db, &config.motor_phi_rbv());

    // HKL channels
    let ch_h = DbChannel::new(&db, &format!("{p}H"));
    let ch_k = DbChannel::new(&db, &format!("{p}K"));
    let ch_l = DbChannel::new(&db, &format!("{p}L"));
    let ch_h_rbv = DbChannel::new(&db, &format!("{p}H_RBV"));
    let ch_k_rbv = DbChannel::new(&db, &format!("{p}K_RBV"));
    let ch_l_rbv = DbChannel::new(&db, &format!("{p}L_RBV"));

    // Trial angle channels
    let ch_tth = DbChannel::new(&db, &format!("{p}TTH"));
    let ch_th = DbChannel::new(&db, &format!("{p}TH"));
    let ch_chi = DbChannel::new(&db, &format!("{p}CHI"));
    let ch_phi = DbChannel::new(&db, &format!("{p}PHI"));

    // Energy / lambda
    let ch_energy = DbChannel::new(&db, &format!("{p}energy"));
    let _ch_lambda = DbChannel::new(&db, &format!("{p}lambda"));

    // Crystal params
    let ch_a = DbChannel::new(&db, &format!("{p}a"));
    let ch_b = DbChannel::new(&db, &format!("{p}b"));
    let ch_c = DbChannel::new(&db, &format!("{p}c"));
    let ch_alpha = DbChannel::new(&db, &format!("{p}alpha"));
    let ch_beta = DbChannel::new(&db, &format!("{p}beta"));
    let ch_gamma = DbChannel::new(&db, &format!("{p}gamma"));

    // Mode, busy, message
    let _ch_mode = DbChannel::new(&db, &format!("{p}Mode"));
    let ch_busy = DbChannel::new(&db, &format!("{p}Busy"));
    let ch_msg = DbChannel::new(&db, &format!("{p}Msg"));
    let _ch_mot_put = DbChannel::new(&db, &format!("{p}motPut"));
    let _ch_mot_get = DbChannel::new(&db, &format!("{p}motGet"));
    let ch_mot_put_auto = DbChannel::new(&db, &format!("{p}motPut_Auto"));
    let ch_mot_get_auto = DbChannel::new(&db, &format!("{p}motGet_Auto"));

    // Build multi-monitor
    let monitored_pvs: Vec<String> = vec![
        format!("{p}energy"),
        format!("{p}lambda"),
        format!("{p}H"),
        format!("{p}K"),
        format!("{p}L"),
        format!("{p}TTH"),
        format!("{p}TH"),
        format!("{p}CHI"),
        format!("{p}PHI"),
        config.motor_tth.clone(),
        config.motor_th.clone(),
        config.motor_chi.clone(),
        config.motor_phi.clone(),
        config.motor_tth_rbv(),
        format!("{p}Mode"),
        format!("{p}motPut"),
        format!("{p}motGet"),
        format!("{p}motPut_Auto"),
        format!("{p}motGet_Auto"),
    ];
    let mut monitor = DbMultiMonitor::new_filtered(&db, &monitored_pvs, my_origin).await;
    println!(
        "orient: subscribed to {} PVs, {} active",
        monitored_pvs.len(),
        monitor.sub_count()
    );

    // Initialize controller
    let mut ctrl = OrientController::default();

    // Read initial values
    ctrl.energy = ch_energy.get_f64().await;
    ctrl.lambda = if ctrl.energy > SMALL {
        HC / ctrl.energy
    } else {
        0.0
    };
    {
        let a = ch_a.get_f64().await;
        let a = if a > 0.0 { a } else { 5.431 };
        let b = ch_b.get_f64().await;
        let b = if b > 0.0 { b } else { 5.431 };
        let c = ch_c.get_f64().await;
        let c = if c > 0.0 { c } else { 5.431 };
        let alpha = ch_alpha.get_f64().await;
        let alpha = if alpha > 0.0 { alpha } else { 90.0 };
        let beta = ch_beta.get_f64().await;
        let beta = if beta > 0.0 { beta } else { 90.0 };
        let gamma = ch_gamma.get_f64().await;
        let gamma = if gamma > 0.0 { gamma } else { 90.0 };
        ctrl.lattice = LatticeParams {
            a,
            b,
            c,
            alpha,
            beta,
            gamma,
        };
    }
    ctrl.mot_put_auto = ch_mot_put_auto.get_i16().await as i32 != 0;
    ctrl.mot_get_auto = ch_mot_get_auto.get_i16().await as i32 != 0;

    // Initial A0 + OMTX
    ctrl.recalc_a0();
    ctrl.recalc_omtx();

    let _ = ch_msg.put_string("Orient initialized").await;
    tracing::info!("orient state machine running for {p}");

    // PV name constants
    let pv_energy = format!("{p}energy");
    let pv_lambda = format!("{p}lambda");
    let pv_h = format!("{p}H");
    let pv_k = format!("{p}K");
    let pv_l = format!("{p}L");
    let pv_tth = format!("{p}TTH");
    let pv_th = format!("{p}TH");
    let pv_chi = format!("{p}CHI");
    let pv_phi = format!("{p}PHI");
    let pv_mot_tth = config.motor_tth.clone();
    let pv_mot_th = config.motor_th.clone();
    let pv_mot_chi = config.motor_chi.clone();
    let pv_mot_phi = config.motor_phi.clone();
    let pv_mot_tth_rbv = config.motor_tth_rbv();
    let pv_mode = format!("{p}Mode");
    let pv_mot_put = format!("{p}motPut");
    let pv_mot_get = format!("{p}motGet");
    let pv_mot_put_auto = format!("{p}motPut_Auto");
    let pv_mot_get_auto = format!("{p}motGet_Auto");

    // Main event loop
    loop {
        let (changed_pv, new_val) = monitor.wait_change().await;

        let event: Option<OrientEvent> = if changed_pv == pv_energy {
            Some(OrientEvent::EnergyChanged(new_val))
        } else if changed_pv == pv_lambda {
            Some(OrientEvent::LambdaChanged(new_val))
        } else if changed_pv == pv_h {
            sleep(Duration::from_millis(20)).await;
            let h = new_val;
            let k = ch_k.get_f64().await;
            let l = ch_l.get_f64().await;
            Some(OrientEvent::HKLChanged { h, k, l })
        } else if changed_pv == pv_k {
            sleep(Duration::from_millis(20)).await;
            let h = ch_h.get_f64().await;
            let k = new_val;
            let l = ch_l.get_f64().await;
            Some(OrientEvent::HKLChanged { h, k, l })
        } else if changed_pv == pv_l {
            sleep(Duration::from_millis(20)).await;
            let h = ch_h.get_f64().await;
            let k = ch_k.get_f64().await;
            let l = new_val;
            Some(OrientEvent::HKLChanged { h, k, l })
        } else if changed_pv == pv_tth
            || changed_pv == pv_th
            || changed_pv == pv_chi
            || changed_pv == pv_phi
        {
            sleep(Duration::from_millis(20)).await;
            let tth = ch_tth.get_f64().await;
            let th = ch_th.get_f64().await;
            let chi = ch_chi.get_f64().await;
            let phi = ch_phi.get_f64().await;
            Some(OrientEvent::AnglesChanged { tth, th, chi, phi })
        } else if changed_pv == pv_mot_tth
            || changed_pv == pv_mot_th
            || changed_pv == pv_mot_chi
            || changed_pv == pv_mot_phi
        {
            let tth = ch_mot_tth.get_f64().await;
            let th = ch_mot_th.get_f64().await;
            let chi = ch_mot_chi.get_f64().await;
            let phi = ch_mot_phi.get_f64().await;
            Some(OrientEvent::MotorsChanged { tth, th, chi, phi })
        } else if changed_pv == pv_mot_tth_rbv {
            let tth = ch_mot_tth_rbv.get_f64().await;
            let th = ch_mot_th_rbv.get_f64().await;
            let chi = ch_mot_chi_rbv.get_f64().await;
            let phi = ch_mot_phi_rbv.get_f64().await;
            Some(OrientEvent::MotorRBVChanged { tth, th, chi, phi })
        } else if changed_pv == pv_mode {
            let m = new_val as i32;
            let constraint = match m {
                1 => Constraint::PhiConst,
                2 => Constraint::MinChiPhiMinus90,
                _ => Constraint::OmegaZero,
            };
            Some(OrientEvent::ModeChanged(constraint))
        } else if changed_pv == pv_mot_put {
            if new_val as i32 != 0 {
                Some(OrientEvent::MotPut)
            } else {
                None
            }
        } else if changed_pv == pv_mot_get {
            if new_val as i32 != 0 {
                Some(OrientEvent::MotGet)
            } else {
                None
            }
        } else if changed_pv == pv_mot_put_auto {
            ctrl.mot_put_auto = new_val as i32 != 0;
            None
        } else if changed_pv == pv_mot_get_auto {
            ctrl.mot_get_auto = new_val as i32 != 0;
            None
        } else {
            None
        };

        if let Some(ev) = event {
            let actions = ctrl.step(ev);

            // Apply actions
            if let Some(msg) = &actions.message {
                let _ = ch_msg.put_string(msg.as_str()).await;
            }
            if let Some(a0) = actions.write_a0 {
                let _ = ch_a.put_f64(a0[0][0]).await;
            }
            if let Some(hkl) = actions.write_hkl {
                let _ = ch_h.put_f64(hkl[0]).await;
                let _ = ch_k.put_f64(hkl[1]).await;
                let _ = ch_l.put_f64(hkl[2]).await;
            }
            if let Some(ang) = actions.write_angles {
                let _ = ch_tth.put_f64(ang[0]).await;
                let _ = ch_th.put_f64(ang[1]).await;
                let _ = ch_chi.put_f64(ang[2]).await;
                let _ = ch_phi.put_f64(ang[3]).await;
            }
            if let Some(motors) = actions.drive_motors {
                let _ = ch_mot_tth.put_f64_process(motors[0]).await;
                let _ = ch_mot_th.put_f64_process(motors[1]).await;
                let _ = ch_mot_chi.put_f64_process(motors[2]).await;
                let _ = ch_mot_phi.put_f64_process(motors[3]).await;
                ctrl.waiting_for_motors = true;
            }
            if let Some(b) = actions.busy_changed {
                let _ = ch_busy.put_i16(if b { 1 } else { 0 }).await;
            }
            if let Some(rbv) = actions.write_hkl_rbv {
                let _ = ch_h_rbv.put_f64(rbv[0]).await;
                let _ = ch_k_rbv.put_f64(rbv[1]).await;
                let _ = ch_l_rbv.put_f64(rbv[2]).await;
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::math::orient::Constraint;

    const LAMBDA: f64 = 0.572495;

    fn make_controller() -> OrientController {
        let mut ctrl = OrientController::default();
        ctrl.lambda = LAMBDA;
        ctrl.energy = HC / LAMBDA;
        ctrl.lattice = LatticeParams {
            a: 5.431,
            b: 5.431,
            c: 5.431,
            alpha: 90.0,
            beta: 90.0,
            gamma: 90.0,
        };
        ctrl.recalc_a0();
        assert_eq!(ctrl.a0_state, CalcState::Succeeded);
        ctrl
    }

    #[test]
    fn test_a0_calculation() {
        let ctrl = make_controller();
        assert_eq!(ctrl.a0_state, CalcState::Succeeded);
    }

    #[test]
    fn test_a0_fails_with_zero_params() {
        let mut ctrl = OrientController::default();
        ctrl.lambda = LAMBDA;
        ctrl.lattice = LatticeParams {
            a: 0.0,
            b: 5.431,
            c: 5.431,
            alpha: 90.0,
            beta: 90.0,
            gamma: 90.0,
        };
        ctrl.recalc_a0();
        assert_eq!(ctrl.a0_state, CalcState::Failed);
    }

    #[test]
    fn test_omtx_with_two_reflections() {
        let mut ctrl = make_controller();
        ctrl.ref1 = Reflection {
            h: 4.0,
            k: 0.0,
            l: 0.0,
            tth: 24.3414,
            th: 12.1707,
            chi: 0.0,
            phi: 0.0,
        };
        ctrl.ref2 = Reflection {
            h: 0.0,
            k: 4.0,
            l: 0.0,
            tth: 24.3414,
            th: 12.1707,
            chi: 0.0,
            phi: 90.0,
        };
        assert!(ctrl.recalc_omtx());
        assert_eq!(ctrl.omtx_state, CalcState::Succeeded);
        assert!(ctrl.err_angle.abs() < 0.1);
    }

    #[test]
    fn test_omtx_fails_with_zero_hkl() {
        let mut ctrl = make_controller();
        ctrl.ref1 = Reflection::default();
        ctrl.ref2 = Reflection::default();
        assert!(!ctrl.recalc_omtx());
        assert_eq!(ctrl.omtx_state, CalcState::Failed);
    }

    #[test]
    fn test_hkl_to_angles_step() {
        let mut ctrl = make_controller();
        ctrl.ref1 = Reflection {
            h: 4.0,
            k: 0.0,
            l: 0.0,
            tth: 24.3414,
            th: 12.1707,
            chi: 0.0,
            phi: 0.0,
        };
        ctrl.ref2 = Reflection {
            h: 0.0,
            k: 4.0,
            l: 0.0,
            tth: 24.3414,
            th: 12.1707,
            chi: 0.0,
            phi: 90.0,
        };
        ctrl.recalc_omtx();

        let actions = ctrl.step(OrientEvent::HKLChanged {
            h: 1.0,
            k: 2.0,
            l: 3.0,
        });

        let angles = actions.write_angles.expect("should produce angles");
        // TTH should be around 22.75 degrees for (1,2,3) with Si at this wavelength
        assert!((angles[0] - 22.7475).abs() < 0.1, "TTH = {}", angles[0]);
    }

    #[test]
    fn test_angles_to_hkl_step() {
        let mut ctrl = make_controller();
        ctrl.ref1 = Reflection {
            h: 4.0,
            k: 0.0,
            l: 0.0,
            tth: 24.3414,
            th: 12.1707,
            chi: 0.0,
            phi: 0.0,
        };
        ctrl.ref2 = Reflection {
            h: 0.0,
            k: 4.0,
            l: 0.0,
            tth: 24.3414,
            th: 12.1707,
            chi: 0.0,
            phi: 90.0,
        };
        ctrl.recalc_omtx();

        let actions = ctrl.step(OrientEvent::AnglesChanged {
            tth: 24.3414,
            th: 12.1707,
            chi: 0.0,
            phi: 0.0,
        });

        let hkl = actions.write_hkl.expect("should produce HKL");
        assert!((hkl[0] - 4.0).abs() < 0.1, "H = {}", hkl[0]);
        assert!(hkl[1].abs() < 0.1, "K = {}", hkl[1]);
        assert!(hkl[2].abs() < 0.1, "L = {}", hkl[2]);
    }

    #[test]
    fn test_ref_get_copies_current_values() {
        let mut ctrl = make_controller();
        ctrl.h = 1.0;
        ctrl.k = 2.0;
        ctrl.l = 3.0;
        ctrl.tth = 10.0;
        ctrl.th = 5.0;
        ctrl.chi = 20.0;
        ctrl.phi = 30.0;

        let actions = ctrl.step(OrientEvent::RefGet1);
        let r = actions.write_ref1.expect("should produce ref1");
        assert_eq!(r.h, 1.0);
        assert_eq!(r.k, 2.0);
        assert_eq!(r.l, 3.0);
        assert_eq!(r.tth, 10.0);
    }

    #[test]
    fn test_mot_put_drives_motors() {
        let mut ctrl = make_controller();
        ctrl.tth = 24.0;
        ctrl.th = 12.0;
        ctrl.chi = 0.0;
        ctrl.phi = 0.0;

        let actions = ctrl.step(OrientEvent::MotPut);
        let motors = actions.drive_motors.expect("should drive motors");
        assert_eq!(motors[0], 24.0);
        assert!(ctrl.busy);
        assert!(ctrl.waiting_for_motors);
    }

    #[test]
    fn test_mot_get_reads_motors() {
        let mut ctrl = make_controller();
        ctrl.ref1 = Reflection {
            h: 4.0,
            k: 0.0,
            l: 0.0,
            tth: 24.3414,
            th: 12.1707,
            chi: 0.0,
            phi: 0.0,
        };
        ctrl.ref2 = Reflection {
            h: 0.0,
            k: 4.0,
            l: 0.0,
            tth: 24.3414,
            th: 12.1707,
            chi: 0.0,
            phi: 90.0,
        };
        ctrl.recalc_omtx();
        ctrl.mot_tth = 24.3414;
        ctrl.mot_th = 12.1707;
        ctrl.mot_chi = 0.0;
        ctrl.mot_phi = 0.0;

        let actions = ctrl.step(OrientEvent::MotGet);
        let hkl = actions.write_hkl.expect("should produce HKL");
        assert!((hkl[0] - 4.0).abs() < 0.1);
    }

    #[test]
    fn test_energy_change_recalculates() {
        let mut ctrl = make_controller();
        ctrl.ref1 = Reflection {
            h: 4.0,
            k: 0.0,
            l: 0.0,
            tth: 24.3414,
            th: 12.1707,
            chi: 0.0,
            phi: 0.0,
        };
        ctrl.ref2 = Reflection {
            h: 0.0,
            k: 4.0,
            l: 0.0,
            tth: 24.3414,
            th: 12.1707,
            chi: 0.0,
            phi: 90.0,
        };
        ctrl.recalc_omtx();
        ctrl.h = 4.0;
        ctrl.k = 0.0;
        ctrl.l = 0.0;

        let new_energy = HC / 0.6; // different wavelength
        let actions = ctrl.step(OrientEvent::EnergyChanged(new_energy));
        assert!(actions.write_a0.is_some());
        assert!(actions.message.is_some());
    }

    #[test]
    fn test_mode_change_recomputes_angles() {
        let mut ctrl = make_controller();
        ctrl.ref1 = Reflection {
            h: 4.0,
            k: 0.0,
            l: 0.0,
            tth: 24.3414,
            th: 12.1707,
            chi: 0.0,
            phi: 0.0,
        };
        ctrl.ref2 = Reflection {
            h: 0.0,
            k: 4.0,
            l: 0.0,
            tth: 24.3414,
            th: 12.1707,
            chi: 0.0,
            phi: 90.0,
        };
        ctrl.recalc_omtx();
        ctrl.h = 1.0;
        ctrl.k = 2.0;
        ctrl.l = 3.0;

        let actions = ctrl.step(OrientEvent::ModeChanged(Constraint::PhiConst));
        assert!(actions.write_angles.is_some());
    }

    #[test]
    fn test_motor_rbv_to_hkl() {
        let mut ctrl = make_controller();
        ctrl.ref1 = Reflection {
            h: 4.0,
            k: 0.0,
            l: 0.0,
            tth: 24.3414,
            th: 12.1707,
            chi: 0.0,
            phi: 0.0,
        };
        ctrl.ref2 = Reflection {
            h: 0.0,
            k: 4.0,
            l: 0.0,
            tth: 24.3414,
            th: 12.1707,
            chi: 0.0,
            phi: 90.0,
        };
        ctrl.recalc_omtx();

        let rbv = [24.3414, 12.1707, 0.0, 0.0];
        let hkl = ctrl.motor_rbv_to_hkl(&rbv).unwrap();
        assert!((hkl[0] - 4.0).abs() < 0.1);
    }

    #[test]
    fn test_sync_energy_lambda() {
        let mut e = 0.0;
        let mut l = 0.572495;
        sync_energy_lambda(&mut e, &mut l);
        assert!((e - HC / 0.572495).abs() < 0.001);

        let mut e = 21.65;
        let mut l = 0.0;
        sync_energy_lambda(&mut e, &mut l);
        assert!((l - HC / 21.65).abs() < 0.001);
    }
}
