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

/// Map the `Mode` mbbo wire value to a diffractometer [`Constraint`].
///
/// Mirrors the C `orient.h:27-29` enum / `orient.db` `Mode` mbbo menu:
/// `OMEGA_ZERO=0` ("TH=TTH/2"), `MIN_CHI_PHIm90=1` ("CHI~90; PHI~0"),
/// `PHI_CONST=2` ("PHI_CONST"). Any out-of-range value falls back to
/// `OMEGA_ZERO`, matching the C default branch.
fn constraint_from_mode(m: i32) -> Constraint {
    match m {
        1 => Constraint::MinChiPhiMinus90,
        2 => Constraint::PhiConst,
        _ => Constraint::OmegaZero,
    }
}

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
    /// `drive_motors` is the motPut command: every motor is written, not
    /// only those that differ (orient_st.st:888-897 vs :1101-1124).
    pub mot_put: bool,
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
    /// Write motPut_Auto to manual: C does on every bad OMTX
    /// (orient_st.st:557, :651, :958).
    pub mot_put_auto_off: bool,
    /// Reset the OMTX_Calc command once the calculation ran
    /// (orient_st.st:540, :643).
    pub clear_omtx_calc: bool,
    /// The step ends in C's putAll, which clears Busy and the *_busy
    /// records when no motor is left moving (orient_st.st:1132-1141).
    pub put_all: bool,
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
    /// C's bad-OMTX exit: drive switch to manual so a bad matrix does not
    /// drive the motors (orient_st.st:555-560, :649-653, :957-959).
    fn bad_omtx(&mut self, actions: &mut OrientActions) {
        self.mot_put_auto = false;
        actions.mot_put_auto_off = true;
        actions.message = Some("Bad OMTX calc; motPut_Auto set to Manual".into());
    }

    /// The init state's OMTX calculation (orient_st.st:514-561): skipped
    /// unless A0 is good, published only when both reflections are nonzero.
    pub fn init_omtx(&mut self) -> OrientActions {
        let mut actions = OrientActions::default();
        if self.a0_state != CalcState::Succeeded {
            return actions;
        }
        let refs_set = self.ref1.hkl() != [0.0; 3] && self.ref2.hkl() != [0.0; 3];
        actions.clear_omtx_calc = refs_set;
        if self.recalc_omtx() {
            actions.write_omtx = Some(self.omtx);
            actions.message = Some("Successful OMTX calc".into());
        } else {
            if refs_set {
                actions.write_omtx = Some(self.omtx);
            }
            if self.mot_put_auto {
                self.bad_omtx(&mut actions);
            }
        }
        actions
    }

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
                // C `calc_A0` (orient.c:195-200) fills the A0/A0_i arrays with
                // identity when the matrix is singular, and the SNL pvPuts that
                // identity to A0_11..A0_33 (orient_st.st:496-503). Mirror that so
                // a singular recalc publishes identity, not the previous matrix.
                self.a0 = IDENTITY;
                self.a0_inv = IDENTITY;
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
                // C `calc_OMTX` (orient.c:279-285) fills OMTX/OMTX_i with identity
                // on a singular result and the SNL pvPuts that identity to
                // OMTX_11..OMTX_33 (orient_st.st:534-539). Match it: a singular
                // recalc publishes identity, not the stale matrix.
                self.omtx = IDENTITY;
                self.omtx_inv = IDENTITY;
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
        // newMotors always ends in putAll; newHKL and newAngles do only with
        // good matrices (orient_st.st:970-1066).
        let via_new_motors = matches!(
            event,
            OrientEvent::MotorsChanged { .. }
                | OrientEvent::MotorsDone { .. }
                | OrientEvent::MotGet
        );
        let via_new_hkl = matches!(
            event,
            OrientEvent::EnergyChanged(_)
                | OrientEvent::LambdaChanged(_)
                | OrientEvent::LatticeChanged(_)
                | OrientEvent::HKLChanged { .. }
                | OrientEvent::AnglesChanged { .. }
                | OrientEvent::CalcOMTX
                | OrientEvent::ModeChanged(_)
                | OrientEvent::A0MatrixChanged(_)
                | OrientEvent::OMTXMatrixChanged(_)
        );

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
                let ok = self.recalc_omtx();
                actions.clear_omtx_calc = true;
                // C `newOMTX` (orient_st.st:637-642) pvPuts OMTX_11..OMTX_33
                // unconditionally after calc_OMTX — identity on a singular result,
                // the real matrix otherwise — before deciding success/failure.
                actions.write_omtx = Some(self.omtx);
                if ok {
                    // Only init's calculation reports success (orient_st.st:554
                    // vs :646-648).
                    // Recompute angles from current HKL
                    if self.hkl_to_trial_angles() {
                        actions.write_angles = Some([self.tth, self.th, self.chi, self.phi]);
                    }
                } else {
                    self.bad_omtx(&mut actions);
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
                // The db's putBusy sets Busy, not the sequencer
                // (orient_st.st:888-897).
                actions.drive_motors = Some([self.tth, self.th, self.chi, self.phi]);
                actions.mot_put = true;
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
                            }
                            None => {
                                self.omtx_state = CalcState::Failed;
                            }
                        }
                    } else {
                        self.omtx_state = CalcState::Failed;
                    }
                } else {
                    self.omtx_state = CalcState::Failed;
                }
                // newOMTX (orient_st.st:954-961) follows every failure here.
                if self.omtx_state != CalcState::Succeeded {
                    self.bad_omtx(&mut actions);
                }
            }
        }

        actions.put_all = via_new_motors
            || via_new_hkl
                && self.a0_state == CalcState::Succeeded
                && self.omtx_state == CalcState::Succeeded;
        actions
    }
}

/// Clear each set *_busy record (orient_st.st:868-874, :1134-1140).
async fn clear_flag_busy(monitor: &mut DbMultiMonitor, pvs: &[String; 7], chs: &[DbChannel; 7]) {
    for (pv, ch) in pvs.iter().zip(chs) {
        if monitor.var(pv) as i16 != 0 {
            let _ = ch.put_i16_process(0).await;
        }
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

    println!("orient: starting for prefix={}", config.prefix);

    let my_origin = alloc_origin();

    let p = &config.prefix;

    // Connect motor channels
    let pv_mots = [
        config.motor_tth.clone(),
        config.motor_th.clone(),
        config.motor_chi.clone(),
        config.motor_phi.clone(),
    ];
    let ch_mots = pv_mots
        .each_ref()
        .map(|pv| DbChannel::with_origin(&db, pv, my_origin));
    let [ch_mot_tth, ch_mot_th, ch_mot_chi, ch_mot_phi] = &ch_mots;
    // DMOV's return to 1 stands in for pvPutComplete(motX) and, like C's
    // completion callback, raises the motor's flag (orient_st.st:776-800).
    let pv_dmovs = pv_mots.each_ref().map(|pv| {
        let base = pv.split('.').next().unwrap_or(pv);
        format!("{base}.DMOV")
    });
    let ch_dmovs = pv_dmovs
        .each_ref()
        .map(|pv| DbChannel::with_origin(&db, pv, my_origin));
    // C's state PVs (orient_st.st:314-331).
    let axes = ["TTH", "TH", "CHI", "PHI"];
    let ch_waiting4 =
        axes.map(|a| DbChannel::with_origin(&db, &format!("{p}waiting4mot{a}"), my_origin));
    let ch_we_wrote =
        axes.map(|a| DbChannel::with_origin(&db, &format!("{p}weWrote_mot{a}"), my_origin));
    let ch_waiting4_mot = DbChannel::with_origin(&db, &format!("{p}waiting4Mot"), my_origin);
    let ch_load_mot_put = DbChannel::with_origin(&db, &format!("{p}load_motPut"), my_origin);
    let mut waiting4 = [false; 4];
    let mut we_wrote = [false; 4];

    // Motor readback channels
    let ch_mot_tth_rbv = DbChannel::with_origin(&db, &config.motor_tth_rbv(), my_origin);
    let ch_mot_th_rbv = DbChannel::with_origin(&db, &config.motor_th_rbv(), my_origin);
    let ch_mot_chi_rbv = DbChannel::with_origin(&db, &config.motor_chi_rbv(), my_origin);
    let ch_mot_phi_rbv = DbChannel::with_origin(&db, &config.motor_phi_rbv(), my_origin);

    // C writes HKL and the trial angles through the load records, whose
    // PACT disables the *_event records (orient_st.st:1083-1094).
    let ch_h = DbChannel::with_origin(&db, &format!("{p}loadH"), my_origin);
    let ch_k = DbChannel::with_origin(&db, &format!("{p}loadK"), my_origin);
    let ch_l = DbChannel::with_origin(&db, &format!("{p}loadL"), my_origin);
    let ch_h_rbv = DbChannel::with_origin(&db, &format!("{p}H_RBV"), my_origin);
    let ch_k_rbv = DbChannel::with_origin(&db, &format!("{p}K_RBV"), my_origin);
    let ch_l_rbv = DbChannel::with_origin(&db, &format!("{p}L_RBV"), my_origin);

    let ch_tth = DbChannel::with_origin(&db, &format!("{p}loadTTH"), my_origin);
    let ch_th = DbChannel::with_origin(&db, &format!("{p}loadTH"), my_origin);
    let ch_chi = DbChannel::with_origin(&db, &format!("{p}loadCHI"), my_origin);
    let ch_phi = DbChannel::with_origin(&db, &format!("{p}loadPHI"), my_origin);

    // Crystal params
    let ch_a = DbChannel::with_origin(&db, &format!("{p}a"), my_origin);
    let ch_b = DbChannel::with_origin(&db, &format!("{p}b"), my_origin);
    let ch_c = DbChannel::with_origin(&db, &format!("{p}c"), my_origin);
    let ch_alpha = DbChannel::with_origin(&db, &format!("{p}alpha"), my_origin);
    let ch_beta = DbChannel::with_origin(&db, &format!("{p}beta"), my_origin);
    let ch_gamma = DbChannel::with_origin(&db, &format!("{p}gamma"), my_origin);

    // Computed-matrix element display PVs. C copies the A0 / OMTX arrays into
    // the per-element PVs {P}A0_11..A0_33 / {P}OMTX_11..OMTX_33 and pvPut's each
    // (orient_st.st:265-283 ASSIGN_MONITOR, :497-503/534-539 publish). These are
    // display readouts, so a posting put (value + monitor, no process) is the
    // C-faithful call.
    let ch_a0: [[DbChannel; 3]; 3] = std::array::from_fn(|i| {
        std::array::from_fn(|j| {
            DbChannel::with_origin(&db, &format!("{p}A0_{}{}", i + 1, j + 1), my_origin)
        })
    });
    let ch_omtx: [[DbChannel; 3]; 3] = std::array::from_fn(|i| {
        std::array::from_fn(|j| {
            DbChannel::with_origin(&db, &format!("{p}OMTX_{}{}", i + 1, j + 1), my_origin)
        })
    });

    // Mode, busy, message
    let _ch_mode = DbChannel::with_origin(&db, &format!("{p}Mode"), my_origin);
    let ch_busy = DbChannel::with_origin(&db, &format!("{p}Busy"), my_origin);
    let ch_msg = DbChannel::with_origin(&db, &format!("{p}Msg"), my_origin);
    let _ch_mot_put = DbChannel::with_origin(&db, &format!("{p}motPut"), my_origin);
    let _ch_mot_get = DbChannel::with_origin(&db, &format!("{p}motGet"), my_origin);
    let ch_mot_put_auto = DbChannel::with_origin(&db, &format!("{p}motPut_Auto"), my_origin);
    let ch_omtx_calc = DbChannel::with_origin(&db, &format!("{p}OMTX_Calc"), my_origin);

    // Build multi-monitor
    // C's event flags sync to the *_event records; the values behind them
    // are plain ASSIGN_MONITOR (orient_st.st:177-256).
    let names = ["H", "K", "L", "TTH", "TH", "CHI", "PHI", "energy", "lambda"];
    let [
        ev_h,
        ev_k,
        ev_l,
        ev_tth,
        ev_th,
        ev_chi,
        ev_phi,
        ev_energy,
        ev_lambda,
    ] = names.map(|f| format!("{p}{f}_event"));
    let values = names.map(|f| format!("{p}{f}"));
    let mut monitored_pvs: Vec<String> = vec![
        ev_energy.clone(),
        ev_lambda.clone(),
        ev_h.clone(),
        ev_k.clone(),
        ev_l.clone(),
        ev_tth.clone(),
        ev_th.clone(),
        ev_chi.clone(),
        ev_phi.clone(),
    ];
    monitored_pvs.extend(pv_mots.iter().cloned());
    monitored_pvs.extend(pv_dmovs.iter().cloned());
    monitored_pvs.extend([
        config.motor_tth_rbv(),
        format!("{p}Mode"),
        format!("{p}motPut"),
        format!("{p}motGet"),
        format!("{p}OMTX_Calc"),
    ]);
    let mut monitor = DbMultiMonitor::new(&db, &monitored_pvs).await;
    // C `ASSIGN_MONITOR` syncs no event flag (orient_st.st:70-72, :126, :136)
    // and only action code reads them (:557, :993, :1024, :1053), so they are
    // written on arrival and wake nothing.
    let pv_mot_put_auto = format!("{p}motPut_Auto");
    let pv_mot_get_auto = format!("{p}motGet_Auto");
    let pv_busy = format!("{p}Busy");
    monitor
        .add_vars(
            &db,
            &[
                pv_mot_put_auto.clone(),
                pv_mot_get_auto.clone(),
                pv_busy.clone(),
            ],
        )
        .await;
    // The reflections and errAngleThresh are plain ASSIGN_MONITOR too
    // (orient_st.st:222-240, :295), read only by the OMTX calculation.
    let pv_ref = |n: u8| ["H", "K", "L", "TTH", "TH", "CHI", "PHI"].map(|f| format!("{p}{f}{n}"));
    let (pv_ref1, pv_ref2) = (pv_ref(1), pv_ref(2));
    let pv_err_angle_thresh = format!("{p}errAngleThresh");
    monitor.add_vars(&db, &values).await;
    let [
        pv_h,
        pv_k,
        pv_l,
        pv_tth,
        pv_th,
        pv_chi,
        pv_phi,
        pv_energy,
        pv_lambda,
    ] = values;
    // The *_busy records the *_event records set (orient_st.st:179-216).
    let pv_flag_busy = ["H", "K", "L", "TTH", "TH", "CHI", "PHI"].map(|f| format!("{p}{f}_busy"));
    monitor.add_vars(&db, &pv_flag_busy).await;
    let ch_flag_busy = pv_flag_busy
        .each_ref()
        .map(|pv| DbChannel::with_origin(&db, pv, my_origin));
    monitor.add_vars(&db, &pv_ref1).await;
    monitor.add_vars(&db, &pv_ref2).await;
    monitor
        .add_vars(&db, std::slice::from_ref(&pv_err_angle_thresh))
        .await;
    let load_vars = |monitor: &mut DbMultiMonitor, ctrl: &mut OrientController| {
        ctrl.mot_put_auto = monitor.var(&pv_mot_put_auto) as i16 != 0;
        ctrl.mot_get_auto = monitor.var(&pv_mot_get_auto) as i16 != 0;
        ctrl.busy = monitor.var(&pv_busy) as i16 != 0;
        let mut reflection = |pvs: &[String; 7]| {
            let [h, k, l, tth, th, chi, phi] = pvs.each_ref().map(|pv| monitor.var(pv));
            Reflection {
                h,
                k,
                l,
                tth,
                th,
                chi,
                phi,
            }
        };
        ctrl.ref1 = reflection(&pv_ref1);
        ctrl.ref2 = reflection(&pv_ref2);
        ctrl.err_angle_thresh = monitor.var(&pv_err_angle_thresh);
    };
    println!(
        "orient: subscribed to {} PVs, {} active",
        monitored_pvs.len(),
        monitor.sub_count()
    );

    // Initialize controller
    let mut ctrl = OrientController::default();

    // Read initial values
    ctrl.energy = monitor.var(&pv_energy);
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

    // Initial A0 + OMTX
    ctrl.recalc_a0();
    load_vars(&mut monitor, &mut ctrl);
    let mut init_actions = Some(ctrl.init_omtx());

    let _ = ch_msg.put_string_process("Orient initialized").await;
    tracing::info!("orient state machine running for {p}");

    // PV name constants
    let pv_mot_tth_rbv = config.motor_tth_rbv();
    let pv_mode = format!("{p}Mode");
    let pv_mot_put = format!("{p}motPut");
    let pv_mot_get = format!("{p}motGet");
    let pv_omtx_calc = format!("{p}OMTX_Calc");

    // Main event loop
    loop {
        let actions = if let Some(actions) = init_actions.take() {
            actions
        } else {
            let (changed_pv, new_val) = monitor.wait_change().await;

            let event: Option<OrientEvent> = if changed_pv == ev_energy {
                Some(OrientEvent::EnergyChanged(monitor.var(&pv_energy)))
            } else if changed_pv == ev_lambda {
                Some(OrientEvent::LambdaChanged(monitor.var(&pv_lambda)))
            } else if [&ev_h, &ev_k, &ev_l].contains(&&changed_pv) {
                if monitor.var(&pv_busy) as i16 == 0 {
                    let _ = ch_busy.put_i16_process(1).await;
                }
                // newHKL's SYNC_WAIT_SECS takes the rest of a vector write
                // and clears its flags (orient_st.st:978-981).
                sleep(Duration::from_millis(20)).await;
                for ev in [&ev_h, &ev_k, &ev_l] {
                    monitor.clear(ev);
                }
                let [h, k, l] = [&pv_h, &pv_k, &pv_l].map(|pv| monitor.var(pv));
                Some(OrientEvent::HKLChanged { h, k, l })
            } else if [&ev_tth, &ev_th, &ev_chi, &ev_phi].contains(&&changed_pv) {
                if monitor.var(&pv_busy) as i16 == 0 {
                    let _ = ch_busy.put_i16_process(1).await;
                }
                // newAngles, orient_st.st:1013-1017.
                sleep(Duration::from_millis(20)).await;
                for ev in [&ev_tth, &ev_th, &ev_chi, &ev_phi] {
                    monitor.clear(ev);
                }
                let [tth, th, chi, phi] =
                    [&pv_tth, &pv_th, &pv_chi, &pv_phi].map(|pv| monitor.var(pv));
                Some(OrientEvent::AnglesChanged { tth, th, chi, phi })
            } else if let Some((i, dmov_wake)) = pv_mots
                .iter()
                .position(|pv| *pv == changed_pv)
                .map(|i| (i, false))
                .or_else(|| {
                    pv_dmovs
                        .iter()
                        .position(|pv| *pv == changed_pv)
                        .map(|i| (i, true))
                })
            {
                // The motor arms of monitor_all (orient_st.st:801-861).
                let new_motors = if waiting4[i] && ch_dmovs[i].get_i16().await != 0 {
                    waiting4[i] = false;
                    let _ = ch_waiting4[i].put_i16_process(0).await;
                    if we_wrote[i] {
                        we_wrote[i] = false;
                        let _ = ch_we_wrote[i].put_i16_process(0).await;
                    }
                    // checkDone (orient_st.st:1030-1046).
                    let done = monitor.var(&pv_busy) as i16 != 0
                        && ctrl.waiting_for_motors
                        && !waiting4.iter().any(|&w| w);
                    if done {
                        ctrl.waiting_for_motors = false;
                        let _ = ch_waiting4_mot.put_i16_process(0).await;
                        let _ = ch_busy.put_i16_process(0).await;
                        clear_flag_busy(&mut monitor, &pv_flag_busy, &ch_flag_busy).await;
                    }
                    done
                } else if dmov_wake {
                    // DMOV leaving 1 is no C event.
                    false
                } else if we_wrote[i] {
                    we_wrote[i] = false;
                    let _ = ch_we_wrote[i].put_i16_process(0).await;
                    false
                } else {
                    true
                };
                if new_motors {
                    let tth = ch_mot_tth.get_f64().await;
                    let th = ch_mot_th.get_f64().await;
                    let chi = ch_mot_chi.get_f64().await;
                    let phi = ch_mot_phi.get_f64().await;
                    Some(OrientEvent::MotorsChanged { tth, th, chi, phi })
                } else {
                    None
                }
            } else if changed_pv == pv_mot_tth_rbv {
                let tth = ch_mot_tth_rbv.get_f64().await;
                let th = ch_mot_th_rbv.get_f64().await;
                let chi = ch_mot_chi_rbv.get_f64().await;
                let phi = ch_mot_phi_rbv.get_f64().await;
                Some(OrientEvent::MotorRBVChanged { tth, th, chi, phi })
            } else if changed_pv == pv_mode {
                let constraint = constraint_from_mode(new_val as i32);
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
            } else if changed_pv == pv_omtx_calc {
                (new_val as i16 != 0).then_some(OrientEvent::CalcOMTX)
            } else {
                None
            };

            let Some(ev) = event else { continue };
            load_vars(&mut monitor, &mut ctrl);
            ctrl.step(ev)
        };

        // Apply actions
        if let Some(msg) = &actions.message {
            let _ = ch_msg.put_string_process(msg.as_str()).await;
        }
        if let Some(a0) = actions.write_a0 {
            // C: A0[i][j] -> {P}A0_ij, pvPut each (orient_st.st:497-503,
            // 671-679). NOT into {P}a, which is a user lattice-param input.
            for (i, row) in ch_a0.iter().enumerate() {
                for (j, ch) in row.iter().enumerate() {
                    let _ = ch.put_f64_process(a0[i][j]).await;
                }
            }
        }
        if let Some(omtx) = actions.write_omtx {
            // C: OMTX[i][j] -> {P}OMTX_ij, pvPut each (orient_st.st:534-539).
            for (i, row) in ch_omtx.iter().enumerate() {
                for (j, ch) in row.iter().enumerate() {
                    let _ = ch.put_f64_process(omtx[i][j]).await;
                }
            }
        }
        if let Some(hkl) = actions.write_hkl {
            let _ = ch_h.put_f64_process(hkl[0]).await;
            let _ = ch_k.put_f64_process(hkl[1]).await;
            let _ = ch_l.put_f64_process(hkl[2]).await;
        }
        if let Some(ang) = actions.write_angles {
            let _ = ch_tth.put_f64_process(ang[0]).await;
            let _ = ch_th.put_f64_process(ang[1]).await;
            let _ = ch_chi.put_f64_process(ang[2]).await;
            let _ = ch_phi.put_f64_process(ang[3]).await;
        }
        if let Some(motors) = actions.drive_motors {
            // putAll writes only the motors that differ; motPut writes
            // all four (orient_st.st:1097-1129, :888-897).
            for (i, ch) in ch_mots.iter().enumerate() {
                if !actions.mot_put && (ch.get_f64().await - motors[i]).abs() <= SMALL {
                    continue;
                }
                we_wrote[i] = true;
                let _ = ch_we_wrote[i].put_i16_process(1).await;
                waiting4[i] = true;
                let _ = ch_waiting4[i].put_i16_process(1).await;
                let _ = ch.put_f64_process(motors[i]).await;
            }
            if actions.mot_put {
                let _ = ch_load_mot_put.put_i16_process(0).await;
            }
            if actions.mot_put || waiting4.iter().any(|&w| w) {
                ctrl.waiting_for_motors = true;
                let _ = ch_waiting4_mot.put_i16_process(1).await;
                if !actions.mot_put && monitor.var(&pv_busy) as i16 == 0 {
                    let _ = ch_busy.put_i16_process(1).await;
                }
            }
        }
        if actions.put_all && !ctrl.waiting_for_motors {
            if monitor.var(&pv_busy) as i16 != 0 {
                let _ = ch_busy.put_i16_process(0).await;
            }
            clear_flag_busy(&mut monitor, &pv_flag_busy, &ch_flag_busy).await;
        }
        if let Some(b) = actions.busy_changed {
            let _ = ch_busy.put_i16_process(if b { 1 } else { 0 }).await;
        }
        if let Some(rbv) = actions.write_hkl_rbv {
            let _ = ch_h_rbv.put_f64_process(rbv[0]).await;
            let _ = ch_k_rbv.put_f64_process(rbv[1]).await;
            let _ = ch_l_rbv.put_f64_process(rbv[2]).await;
        }
        if actions.clear_omtx_calc {
            let _ = ch_omtx_calc.put_i16_process(0).await;
        }
        if actions.mot_put_auto_off {
            let _ = ch_mot_put_auto.put_i16_process(0).await;
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
    fn test_constraint_from_mode() {
        // C orient.h:27-29 / orient.db Mode mbbo menu ordering.
        assert_eq!(constraint_from_mode(0), Constraint::OmegaZero);
        assert_eq!(constraint_from_mode(1), Constraint::MinChiPhiMinus90);
        assert_eq!(constraint_from_mode(2), Constraint::PhiConst);
        // Out-of-range falls back to the C default branch (OMEGA_ZERO).
        assert_eq!(constraint_from_mode(3), Constraint::OmegaZero);
        assert_eq!(constraint_from_mode(-1), Constraint::OmegaZero);
    }

    #[test]
    fn test_a0_calculation() {
        let ctrl = make_controller();
        assert_eq!(ctrl.a0_state, CalcState::Succeeded);
    }

    #[test]
    fn test_singular_omtx_recalc_sets_identity() {
        // Two identical reflections make the OMTX intermediate matrices
        // singular, so calc_omtx returns None. C calc_OMTX fills OMTX/OMTX_i
        // with identity on a singular result (orient.c:279-285) and the SNL
        // pvPuts that identity; recalc_omtx must do the same so the published
        // OMTX_* PVs read identity, not the previous (stale) matrix.
        let mut ctrl = make_controller();
        // Seed a non-identity OMTX so stale-vs-identity is observable.
        ctrl.omtx = [[2.0, 0.0, 0.0], [0.0, 3.0, 0.0], [0.0, 0.0, 4.0]];
        ctrl.omtx_inv = ctrl.omtx;
        let refl = Reflection {
            h: 1.0,
            k: 1.0,
            l: 0.0,
            tth: 20.0,
            th: 10.0,
            chi: 5.0,
            phi: 0.0,
        };
        ctrl.ref1 = refl.clone();
        ctrl.ref2 = refl; // identical reflection → singular OMTX
        let ok = ctrl.recalc_omtx();
        assert!(!ok);
        assert_eq!(ctrl.omtx_state, CalcState::Failed);
        assert_eq!(ctrl.omtx, IDENTITY);
        assert_eq!(ctrl.omtx_inv, IDENTITY);
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
        assert!(actions.mot_put);
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
    fn test_init_bad_omtx_turns_mot_put_auto_off() {
        // orient_st.st:555-560: only a set motPut_Auto is written.
        let mut ctrl = make_controller();
        let actions = ctrl.init_omtx();
        assert!(!actions.mot_put_auto_off);
        assert!(actions.write_omtx.is_none());
        ctrl.mot_put_auto = true;
        let actions = ctrl.init_omtx();
        assert!(actions.mot_put_auto_off);
        assert!(!ctrl.mot_put_auto);
    }

    #[test]
    fn test_user_omtx_failing_the_check_turns_mot_put_auto_off() {
        // newOMTX (orient_st.st:957-959) after the OMTX_mon arm.
        let mut ctrl = make_controller();
        ctrl.mot_put_auto = true;
        let actions = ctrl.step(OrientEvent::OMTXMatrixChanged([[0.0; 3]; 3]));
        assert_eq!(ctrl.omtx_state, CalcState::Failed);
        assert!(actions.mot_put_auto_off);
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
