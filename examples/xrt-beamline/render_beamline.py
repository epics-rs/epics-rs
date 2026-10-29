#!/usr/bin/env python
"""
XRT beamline rendering: Undulator → DCM Si(111) → HFM → VFM → Sample

Matches the Rust xrt-beamline example configuration:
  - Undulator: 3 GeV, period=20mm, gap=15mm
  - DCM: Si(111), theta=12 deg, fixed exit offset=15mm
  - HFM: toroid, positionRoll=pi/2, pitch=3 mrad (horizontal focusing)
  - VFM: toroid, positionRoll=pi, pitch=3 mrad (vertical focusing)
  - Sample: screen at 33 m from source

Usage:
    python render_beamline.py    # a Python with xrt
"""

import os
import numpy as np

import xrt.backends.raycing as raycing
import xrt.backends.raycing.sources as rs
import xrt.backends.raycing.oes as roe
import xrt.backends.raycing.screens as rsc
import xrt.backends.raycing.materials as rm
import xrt.backends.raycing.run as rr
import xrt.plotter as xrtp
import xrt.runner as xrtr

# ============================================================================
# Beamline geometry (matches Rust SimConfig defaults)
# ============================================================================

SOURCE_TO_DCM = 25_000.0    # 25 m [mm]
DCM_FIXED_OFFSET = 15.0     # fixed exit offset [mm]
DCM_TO_HFM = 2_000.0        # 2 m [mm]
HFM_TO_VFM = 3_000.0        # 3 m [mm]
VFM_TO_SAMPLE = 3_000.0     # 3 m [mm]

# DCM Bragg angle → photon energy (8 keV)
DCM_THETA_DEG = 14.31
SI_D = 3.1356  # Si(111) d-spacing [A]
DCM_ENERGY = 12398.42 / (2 * SI_D * np.sin(np.radians(DCM_THETA_DEG)))

# Source energy: broad band centered on DCM energy
E_BW = 0.02  # 2% bandwidth

# Mirror grazing angles
HFM_PITCH_MRAD = 3.0
VFM_PITCH_MRAD = 3.0

# Coddington equations for stigmatic focusing at sample:
#   R = 2*p*q / (sin(α) * (p+q))   (meridional)
#   r = 2*sin(α)*p*q / (p+q)       (sagittal)
# HFM: p=27m (source→HFM), q=6m (HFM→sample)
p_hfm = SOURCE_TO_DCM + DCM_TO_HFM         # 27,000 mm
q_hfm = HFM_TO_VFM + VFM_TO_SAMPLE         # 6,000 mm
alpha_hfm = HFM_PITCH_MRAD * 1e-3
HFM_R_MAJOR = 2*p_hfm*q_hfm / (alpha_hfm * (p_hfm + q_hfm))
HFM_R_MINOR = 1e9  # no sagittal focusing — HFM focuses horizontally only

# VFM: p=30m (source→VFM), q=3m (VFM→sample)
p_vfm = SOURCE_TO_DCM + DCM_TO_HFM + HFM_TO_VFM  # 30,000 mm
q_vfm = VFM_TO_SAMPLE                              # 3,000 mm
alpha_vfm = VFM_PITCH_MRAD * 1e-3
VFM_R_MAJOR = 2*p_vfm*q_vfm / (alpha_vfm * (p_vfm + q_vfm))
VFM_R_MINOR = 1e9  # no sagittal focusing — VFM focuses vertically only

print(f"HFM: R = {HFM_R_MAJOR/1e6:.2f} km (meridional only)")
print(f"VFM: R = {VFM_R_MAJOR/1e6:.2f} km (meridional only)")

# Positions along beamline (y-axis)
Y_SOURCE = 0.0
Y_DCM = SOURCE_TO_DCM
Y_HFM = Y_DCM + DCM_TO_HFM
Y_VFM = Y_HFM + HFM_TO_VFM
Y_SAMPLE = Y_VFM + VFM_TO_SAMPLE

# After the DCM the beam stays near z=0 in the global frame.
# The fixed exit offset is internal to the DCM geometry and should not be
# reused as the downstream beam center.
Z_AFTER_DCM = 0.0

# HFM deflects beam horizontally by 2*pitch (x-direction)
HFM_DEFLECTION = 2.0 * HFM_PITCH_MRAD * 1e-3  # rad
# VFM deflects beam vertically by 2*pitch (z-direction, downward)
VFM_DEFLECTION = 2.0 * VFM_PITCH_MRAD * 1e-3   # rad

NRAYS = 100_000

# Output directory
OUT_DIR = os.path.join(os.path.dirname(os.path.abspath(__file__)), "render_output")
os.makedirs(OUT_DIR, exist_ok=True)

print(f"DCM theta = {DCM_THETA_DEG} deg, E = {DCM_ENERGY:.1f} eV")
print(f"Fixed exit offset = {DCM_FIXED_OFFSET} mm")


# ============================================================================
# Materials
# ============================================================================

crystal_si1 = rm.CrystalSi(hkl=(1, 1, 1), tK=297.15)
crystal_si2 = rm.CrystalSi(hkl=(1, 1, 1), tK=297.15)
mirror_si = rm.Material('Si', rho=2.33, kind='mirror')


# ============================================================================
# Build beamline
# ============================================================================

def build_beamline():
    bl = raycing.BeamLine(alignE=DCM_ENERGY)

    # --- Source ---
    bl.source = rs.GeometricSource(
        bl, name='Undulator',
        center=[0, Y_SOURCE, 0],
        nrays=NRAYS,
        distx='normal', dx=0.3,
        distz='normal', dz=0.02,
        distxprime='normal', dxprime=50e-6,
        distzprime='normal', dzprime=20e-6,
        distE='flat',
        energies=(DCM_ENERGY * (1 - E_BW/2), DCM_ENERGY * (1 + E_BW/2)),
        polarization='horizontal',
    )

    # --- DCM Si(111) with fixed exit offset ---
    # DCM: crystal 60×30 mm
    bl.dcm = roe.DCM(
        bl, name='DCM',
        center=[0, Y_DCM, 0],
        material=(crystal_si1,),
        material2=(crystal_si2,),
        bragg='auto',
        fixedOffset=DCM_FIXED_OFFSET,
        limPhysX=(-30, 30),
        limPhysY=(-15, 15),
        limPhysX2=(-30, 30),
        limPhysY2=(-15, 15),
    )

    # --- HFM: horizontally focusing mirror (same type as VFM) ---
    # positionRoll=pi/2 rotates mirror to deflect horizontally
    # HFM: 1000mm long × 60mm wide
    bl.hfm = roe.BentFlatMirror(
        bl, name='HFM',
        center=[0, Y_HFM, DCM_FIXED_OFFSET],
        material=(mirror_si,),
        pitch=HFM_PITCH_MRAD * 1e-3,
        R=HFM_R_MAJOR,
        positionRoll=np.pi/2,  # horizontal deflection
        limPhysX=(-30, 30),
        limPhysY=(-500, 500),
    )

    # --- VFM: vertically focusing mirror ---
    # After HFM, beam is shifted in x by HFM deflection
    x_after_hfm = HFM_DEFLECTION * HFM_TO_VFM  # x offset at VFM position

    # VFM: 600mm long × 60mm wide
    bl.vfm = roe.BentFlatMirror(
        bl, name='VFM',
        center=[x_after_hfm, Y_VFM, Z_AFTER_DCM],
        material=(mirror_si,),
        pitch=VFM_PITCH_MRAD * 1e-3,
        R=VFM_R_MAJOR,
        positionRoll=np.pi,  # vertical deflection (downward)
        limPhysX=(-30, 30),
        limPhysY=(-300, 300),
    )

    # --- Beam direction vectors after each element ---
    # After HFM: beam deflected in +x by 2*pitch
    # direction = (sin(2*pitchH), cos(2*pitchH), 0) ≈ (2*pitchH, 1, 0)
    hfm_defl = 2.0 * HFM_PITCH_MRAD * 1e-3
    # After VFM: additionally deflected in -z by 2*pitch
    vfm_defl = 2.0 * VFM_PITCH_MRAD * 1e-3

    # Beam direction after HFM (normalized)
    beam_dir_hfm = np.array([hfm_defl, 1.0, 0.0])
    beam_dir_hfm /= np.linalg.norm(beam_dir_hfm)

    # Beam direction after VFM (normalized)
    beam_dir_vfm = np.array([hfm_defl, 1.0, -vfm_defl])
    beam_dir_vfm /= np.linalg.norm(beam_dir_vfm)

    # Screen orientation: z-axis = beam direction (normal to screen)
    # x-axis = horizontal perpendicular to beam
    def screen_axes(beam_dir):
        """Compute screen x,z local axes perpendicular to beam direction."""
        # z_local = beam direction (screen normal, pointing along beam)
        z_loc = beam_dir / np.linalg.norm(beam_dir)
        # x_local = horizontal, perpendicular to beam (global_z × beam_dir)
        x_loc = np.cross([0, 0, 1], z_loc)
        x_norm = np.linalg.norm(x_loc)
        if x_norm > 1e-10:
            x_loc /= x_norm
        else:
            x_loc = np.array([1, 0, 0])
        # z_screen = x_loc × z_loc (vertical on screen)
        z_scr = np.cross(z_loc, x_loc)
        return list(x_loc), list(z_scr)

    # --- Screens ---
    d_screen = 500.0  # distance from OE to screen

    bl.fsmDCM = rsc.Screen(
        bl, name='After DCM',
        center=[0, Y_DCM + d_screen, Z_AFTER_DCM])

    # After HFM screen
    x_hfm_s, z_hfm_s = screen_axes(beam_dir_hfm)
    bl.fsmHFM = rsc.Screen(
        bl, name='After HFM',
        center=[hfm_defl * d_screen, Y_HFM + d_screen, Z_AFTER_DCM],
        x=x_hfm_s, z=z_hfm_s)

    # After VFM screen
    x_vfm_s, z_vfm_s = screen_axes(beam_dir_vfm)
    bl.fsmVFM = rsc.Screen(
        bl, name='After VFM',
        center=[x_after_hfm + hfm_defl * d_screen,
                Y_VFM + d_screen,
                Z_AFTER_DCM - vfm_defl * d_screen],
        x=x_vfm_s, z=z_vfm_s)

    # Sample screen
    x_at_sample = x_after_hfm + hfm_defl * VFM_TO_SAMPLE
    z_at_sample = Z_AFTER_DCM - vfm_defl * VFM_TO_SAMPLE
    bl.fsmSample = rsc.Screen(
        bl, name='Sample',
        center=[x_at_sample, Y_SAMPLE, z_at_sample],
        x=x_vfm_s, z=z_vfm_s)

    return bl


# ============================================================================
# Run process
# ============================================================================

def run_process(bl):
    beamSource = bl.source.shine()

    beamDCMglobal, beamDCMlocal1, beamDCMlocal2 = bl.dcm.double_reflect(
        beamSource)

    beamFsmDCM = bl.fsmDCM.expose(beamDCMglobal)

    beamHFMglobal, beamHFMlocal = bl.hfm.reflect(beamDCMglobal)

    beamFsmHFM = bl.fsmHFM.expose(beamHFMglobal)

    beamVFMglobal, beamVFMlocal = bl.vfm.reflect(beamHFMglobal)

    beamFsmVFM = bl.fsmVFM.expose(beamVFMglobal)
    beamSample = bl.fsmSample.expose(beamVFMglobal)

    return {
        'beamSource': beamSource,
        'beamDCMglobal': beamDCMglobal,
        'beamDCMlocal1': beamDCMlocal1,
        'beamDCMlocal2': beamDCMlocal2,
        'beamFsmDCM': beamFsmDCM,
        'beamHFMglobal': beamHFMglobal,
        'beamHFMlocal': beamHFMlocal,
        'beamFsmHFM': beamFsmHFM,
        'beamVFMglobal': beamVFMglobal,
        'beamVFMlocal': beamVFMlocal,
        'beamFsmVFM': beamFsmVFM,
        'beamSample': beamSample,
    }

rr.run_process = run_process


# ============================================================================
# Plots
# ============================================================================

def define_plots():
    plots = []

    # --- After DCM ---
    plot_dcm = xrtp.XYCPlot(
        'beamFsmDCM', (1,),
        xaxis=xrtp.XYCAxis(r'$x$', 'mm', limits=[-5, 5]),
        yaxis=xrtp.XYCAxis(r'$z$', 'mm', limits=[-5, 5]),
        caxis=xrtp.XYCAxis('energy', 'eV'),
        title='After DCM')
    plot_dcm.saveName = os.path.join(OUT_DIR, '01_after_dcm.png')
    plots.append(plot_dcm)

    # --- After DCM: energy ---
    plot_dcm_e = xrtp.XYCPlot(
        'beamFsmDCM', (1,),
        xaxis=xrtp.XYCAxis(r'$x$', 'mm', limits=[-5, 5]),
        yaxis=xrtp.XYCAxis('energy', 'eV'),
        title='After DCM - Energy vs X')
    plot_dcm_e.saveName = os.path.join(OUT_DIR, '02_after_dcm_energy.png')
    plots.append(plot_dcm_e)

    # --- HFM footprint ---
    plot_hfm_fp = xrtp.XYCPlot(
        'beamHFMlocal', (1,),
        xaxis=xrtp.XYCAxis(r'$x$', 'mm', limits=[-30, 30]),
        yaxis=xrtp.XYCAxis(r'$y$', 'mm', limits=[-300, 300]),
        title='HFM footprint')
    plot_hfm_fp.saveName = os.path.join(OUT_DIR, '03_hfm_footprint.png')
    plots.append(plot_hfm_fp)

    # --- After HFM ---
    plot_hfm = xrtp.XYCPlot(
        'beamFsmHFM', (1,),
        xaxis=xrtp.XYCAxis(r'$x$', 'mm', limits=[-5, 5]),
        yaxis=xrtp.XYCAxis(r'$z$', 'mm', limits=[-5, 5]),
        caxis=xrtp.XYCAxis('energy', 'eV'),
        title='After HFM')
    plot_hfm.saveName = os.path.join(OUT_DIR, '04_after_hfm.png')
    plots.append(plot_hfm)

    # --- VFM footprint ---
    plot_vfm_fp = xrtp.XYCPlot(
        'beamVFMlocal', (1,),
        xaxis=xrtp.XYCAxis(r'$x$', 'mm', limits=[-30, 30]),
        yaxis=xrtp.XYCAxis(r'$y$', 'mm', limits=[-300, 300]),
        title='VFM footprint')
    plot_vfm_fp.saveName = os.path.join(OUT_DIR, '05_vfm_footprint.png')
    plots.append(plot_vfm_fp)

    # --- After VFM ---
    plot_vfm = xrtp.XYCPlot(
        'beamFsmVFM', (1,),
        xaxis=xrtp.XYCAxis(r'$x$', 'mm', limits=[-5, 5]),
        yaxis=xrtp.XYCAxis(r'$z$', 'mm', limits=[-5, 5]),
        caxis=xrtp.XYCAxis('energy', 'eV'),
        title='After VFM')
    plot_vfm.saveName = os.path.join(OUT_DIR, '06_after_vfm.png')
    plots.append(plot_vfm)

    # --- Sample ---
    plot_sample = xrtp.XYCPlot(
        'beamSample', (1,),
        xaxis=xrtp.XYCAxis(r'$x$', 'mm', limits=[-5, 5]),
        yaxis=xrtp.XYCAxis(r'$z$', 'mm', limits=[-5, 5]),
        caxis=xrtp.XYCAxis('energy', 'eV'),
        title='Sample')
    plot_sample.saveName = os.path.join(OUT_DIR, '07_sample.png')
    plots.append(plot_sample)

    # --- Sample micro-zoom ---
    plot_sample_um = xrtp.XYCPlot(
        'beamSample', (1,),
        xaxis=xrtp.XYCAxis(r'$x$', u'µm', factor=1e3, limits=[-500, 500]),
        yaxis=xrtp.XYCAxis(r'$z$', u'µm', factor=1e3, limits=[-500, 500]),
        caxis=xrtp.XYCAxis('energy', 'eV'),
        title=u'Sample (µm scale)')
    plot_sample_um.saveName = os.path.join(OUT_DIR, '08_sample_micro.png')
    plots.append(plot_sample_um)

    return plots


# ============================================================================
# Main
# ============================================================================

def main():
    bl = build_beamline()
    plots = define_plots()

    print(f"\nBeamline layout:")
    print(f"  Source:  y = {Y_SOURCE/1000:.1f} m")
    print(f"  DCM:    y = {Y_DCM/1000:.1f} m  (fixed offset = {DCM_FIXED_OFFSET} mm)")
    print(f"  HFM:    y = {Y_HFM/1000:.1f} m  (pitch = {HFM_PITCH_MRAD} mrad, horizontal)")
    print(f"  VFM:    y = {Y_VFM/1000:.1f} m  (pitch = {VFM_PITCH_MRAD} mrad, vertical)")
    print(f"  Sample: y = {Y_SAMPLE/1000:.1f} m")
    print(f"\nE = {DCM_ENERGY:.1f} eV")
    print(f"\nOutput: {OUT_DIR}")
    print(f"Running {NRAYS} rays x 4 repeats...\n")

    xrtr.run_ray_tracing(
        plots, repeats=4,
        beamLine=bl,
        processes=1,
    )


if __name__ == '__main__':
    main()
