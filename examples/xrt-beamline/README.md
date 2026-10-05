# XRT Beamline

Undulator → DCM Si(111) → HFM → VFM → Sample beamline simulation.

Moving motors triggers real-time [xrt-rs](https://github.com/physwkim/xrt-rs) ray tracing
and publishes the beam profile at the sample position as an AreaDetector image.

## Coordinate System

```
           z (vertical, up)
           ↑
           |
           |        ← ring center (-x)
           |
     ──────+──────→ x (horizontal, outboard)
          /
         /
        ↙
       y (beam direction, downstream)
```

Viewed from above (+z), electrons circulate **counterclockwise** (PLS-II convention).
The Lorentz force bends electrons toward the ring center (**-x**), so the beam
exits tangentially in the **+y** direction.

| Axis | Direction | Role |
|------|-----------|------|
| **y** | beam downstream | Beam propagation. Defines element positions along the beamline |
| **x** | horizontal, ⊥ beam | HFM deflects beam horizontally |
| **z** | vertical (up) | DCM offsets beam vertically, VFM deflects beam vertically |

## Beamline Layout

```
y=0m        y=25m       y=27m       y=30m       y=33m
Source ────→ DCM ───────→ HFM ───────→ VFM ───────→ Sample
             z+15mm       x deflect    z deflect    x=+36mm
             (offset)     (6mrad)      (6mrad)      z=-3mm
```

| Element | Position | Function |
|---------|----------|----------|
| **Undulator** | 0 m | X-ray source. Gap → photon energy |
| **DCM Si(111)** | 25 m | Energy selection. Bragg angle θ=12° → 9509 eV. Fixed exit offset 15 mm (+z) |
| **HFM** | 27 m | Horizontal focusing. pitch=3 mrad, positionRoll=π/2 |
| **VFM** | 30 m | Vertical focusing. pitch=3 mrad, positionRoll=π |
| **Sample** | 33 m | AreaDetector screen |

## Mirror Focusing (Coddington Equations)

The meridional bending radius R for each mirror is determined by the Coddington equation:

```
1/p + 1/q = 2·sin(α) / R

→  R = 2·p·q / (sin(α)·(p+q))
```

- p: source → mirror distance
- q: mirror → sample distance
- α: grazing angle

Each mirror focuses in one direction only (meridional), so the sagittal radius r
is set to ∞ (r_minor = 1e9) to disable sagittal focusing.

| Mirror | p | q | α | R (meridional) | Demag (q/p) |
|--------|---|---|---|----------------|-------------|
| **HFM** | 27 m | 6 m | 3 mrad | 3.27 km | 0.22 |
| **VFM** | 30 m | 3 m | 3 mrad | 1.82 km | 0.10 |

Beam size at sample (demagnification):

- x: source σ_x=0.3 mm × 0.22 = **66 µm**
- z: source σ_z=0.02 mm × 0.10 = **2 µm**

## Motors (25 total)

### Undulator (3)

| Motor | PV | Default | Unit | Description |
|-------|----|---------|------|-------------|
| und_gap | `bl:und:gap` | 15 | mm | Gap → photon energy |
| und_x | `bl:und:x` | 0 | mm | Horizontal position |
| und_z | `bl:und:z` | 0 | mm | Vertical position |

### DCM (6)

| Motor | PV | Default | Unit | Description |
|-------|----|---------|------|-------------|
| dcm_theta | `bl:dcm:theta` | 12 | deg | Bragg angle → energy selection |
| dcm_theta2 | `bl:dcm:theta2` | 0 | arcsec | 2nd crystal fine adjust |
| dcm_y | `bl:dcm:y` | 15 | mm | Crystal gap (fixed exit offset) |
| dcm_chi1 | `bl:dcm:chi1` | 0 | mrad | 1st crystal roll |
| dcm_chi2 | `bl:dcm:chi2` | 0 | mrad | 2nd crystal roll |
| dcm_z | `bl:dcm:z` | 0 | mm | Translation |

### HFM (8)

| Motor | PV | Default | Unit | Description |
|-------|----|---------|------|-------------|
| hfm_pitch | `bl:hfm:pitch` | 3 | mrad | Grazing angle |
| hfm_roll | `bl:hfm:roll` | 0 | mrad | Roll |
| hfm_yaw | `bl:hfm:yaw` | 0 | mrad | Yaw |
| hfm_x | `bl:hfm:x` | 0 | mm | Horizontal translation |
| hfm_y | `bl:hfm:y` | 0 | mm | Vertical translation |
| hfm_z | `bl:hfm:z` | 0 | mm | Longitudinal translation |
| hfm_rmaj | `bl:hfm:rmaj` | 5e6 | mm | Meridional bending radius |
| hfm_rmin | `bl:hfm:rmin` | 50 | mm | Sagittal radius |

### VFM (8)

| Motor | PV | Default | Unit | Description |
|-------|----|---------|------|-------------|
| vfm_pitch | `bl:vfm:pitch` | 3 | mrad | Grazing angle |
| vfm_roll | `bl:vfm:roll` | 0 | mrad | Roll |
| vfm_yaw | `bl:vfm:yaw` | 0 | mrad | Yaw |
| vfm_x | `bl:vfm:x` | 0 | mm | Horizontal translation |
| vfm_y | `bl:vfm:y` | 0 | mm | Vertical translation |
| vfm_z | `bl:vfm:z` | 0 | mm | Longitudinal translation |
| vfm_rmaj | `bl:vfm:rmaj` | 5e6 | mm | Meridional bending radius |
| vfm_rmin | `bl:vfm:rmin` | 50 | mm | Sagittal radius |

## Simulation Readback PVs

| PV | Unit | Description |
|----|------|-------------|
| `bl:xrt:cam1:SrcEnergy_RBV` | eV | Undulator source energy |
| `bl:xrt:cam1:DcmEnergy_RBV` | eV | DCM selected energy |
| `bl:xrt:cam1:Efficiency_RBV` | % | Total beamline transmission |
| `bl:xrt:cam1:Flux_RBV` | — | Total intensity at sample |
| `bl:xrt:cam1:CentroidX_RBV` | mm | Beam centroid x |
| `bl:xrt:cam1:CentroidZ_RBV` | mm | Beam centroid z |
| `bl:xrt:cam1:FWHMX_RBV` | mm | Beam FWHM x |
| `bl:xrt:cam1:FWHMZ_RBV` | mm | Beam FWHM z |
| `bl:xrt:cam1:NRays_RBV` | — | Number of good rays at sample |

## Detector Acquisition Task API

The XRT detector's acquisition task is fully async. A driver author only needs three types from `ad_core_rs::plugin::channel`:

| Type | Purpose |
|------|---------|
| `PortHandle` | Read/write parameters via `read_int32().await`, `set_params_and_notify().await` |
| `ArrayPublisher` | Publish a generated frame to downstream plugins: `publisher.publish(frame).await` |
| `QueuedArrayCounter` | Wait until in-flight frames drain at end of acquisition |

The acquisition loop runs inside a `current_thread` tokio runtime created by the task thread. All I/O is async and reliable — there are no lossy or blocking APIs in the data path.

## Build & Run

### EPICS IOC (Rust)

```bash
cargo build -p xrt-beamline --features ioc --release
./target/release/xrt-ioc examples/xrt-beamline/ioc/st.cmd
```

```bash
# Start detector (continuous, ~10 Hz)
caput bl:xrt:cam1:ArrayCallbacks 1
caput bl:xrt:cam1:Acquire 1

# Move motors
caput bl:dcm:theta 15
caput bl:und:gap 20
caput bl:hfm:pitch 4
caput bl:vfm:pitch 4

# Monitor
camonitor bl:xrt:cam1:DcmEnergy_RBV
camonitor bl:xrt:cam1:FWHMX_RBV
camonitor bl:xrt:cam1:Efficiency_RBV
```

### Python Rendering (xrt)

```bash
python examples/xrt-beamline/render_beamline.py
```

Output: `examples/xrt-beamline/render_output/*.png`
