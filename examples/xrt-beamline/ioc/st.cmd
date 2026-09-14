epicsEnvSet("PREFIX", "bl:")

# ===== Simulation parameters =====
epicsEnvSet("XRT_NRAYS",      "5000")
# 3.1 um/pixel, +/-0.2 mm screen, 128x128 (matches Python viewer)
epicsEnvSet("XRT_SCREEN_NX",  "128")
epicsEnvSet("XRT_SCREEN_NZ",  "128")
epicsEnvSet("XRT_SCREEN_DX",  "0.2")
epicsEnvSet("XRT_SCREEN_DZ",  "0.2")
epicsEnvSet("XRT_SIZE_X",     "128")
epicsEnvSet("XRT_SIZE_Y",     "128")
epicsEnvSet("XRT_MAX_MEMORY", "50000000")

# ===== Undulator motors =====
# simMotorCreate(port, lowLimit, highLimit, pollMs)
simMotorCreate("und_gap", 5, 200, 100)
simMotorCreate("und_x", -5, 5, 100)
simMotorCreate("und_z", -5, 5, 100)

# ===== DCM motors =====
simMotorCreate("dcm_theta", 4, 80, 100)
simMotorCreate("dcm_theta2", -100, 100, 100)
simMotorCreate("dcm_y", 0, 50, 100)
simMotorCreate("dcm_chi1", -5, 5, 100)
simMotorCreate("dcm_chi2", -5, 5, 100)
simMotorCreate("dcm_z", -200, 200, 100)

# ===== HFM motors =====
simMotorCreate("hfm_pitch", 1, 10, 100)
simMotorCreate("hfm_roll", -5, 5, 100)
simMotorCreate("hfm_yaw", -5, 5, 100)
simMotorCreate("hfm_x", -10, 10, 100)
simMotorCreate("hfm_y", -10, 10, 100)
simMotorCreate("hfm_z", -100, 100, 100)
simMotorCreate("hfm_rmaj", 100000, 50000000, 100)
simMotorCreate("hfm_rmin", 10, 2000000000, 100)

# ===== VFM motors =====
simMotorCreate("vfm_pitch", 1, 10, 100)
simMotorCreate("vfm_roll", -5, 5, 100)
simMotorCreate("vfm_yaw", -5, 5, 100)
simMotorCreate("vfm_x", -10, 10, 100)
simMotorCreate("vfm_y", -10, 10, 100)
simMotorCreate("vfm_z", -100, 100, 100)
simMotorCreate("vfm_rmaj", 100000, 50000000, 100)
simMotorCreate("vfm_rmin", 10, 2000000000, 100)

# Configure XRT beamline simulation
xrtBeamlineConfig()

# ===== Load Undulator motor records =====
dbLoadRecords("$(MOTOR)/motor.template", "P=$(PREFIX),M=und:gap,PORT=und_gap,VELO=0.5,ACCL=0.5,HLM=200,LLM=5")
dbLoadRecords("$(MOTOR)/motor.template", "P=$(PREFIX),M=und:x,PORT=und_x,VELO=0.2,ACCL=0.2,HLM=5,LLM=-5")
dbLoadRecords("$(MOTOR)/motor.template", "P=$(PREFIX),M=und:z,PORT=und_z,VELO=0.2,ACCL=0.2,HLM=5,LLM=-5")

# ===== Load DCM motor records =====
dbLoadRecords("$(MOTOR)/motor.template", "P=$(PREFIX),M=dcm:theta,PORT=dcm_theta,VELO=0.2,ACCL=0.5,HLM=80,LLM=4")
dbLoadRecords("$(MOTOR)/motor.template", "P=$(PREFIX),M=dcm:theta2,PORT=dcm_theta2,VELO=1,ACCL=0.2,HLM=100,LLM=-100")
dbLoadRecords("$(MOTOR)/motor.template", "P=$(PREFIX),M=dcm:y,PORT=dcm_y,VELO=0.5,ACCL=0.3,HLM=50,LLM=0")
dbLoadRecords("$(MOTOR)/motor.template", "P=$(PREFIX),M=dcm:chi1,PORT=dcm_chi1,VELO=0.1,ACCL=0.2,HLM=5,LLM=-5")
dbLoadRecords("$(MOTOR)/motor.template", "P=$(PREFIX),M=dcm:chi2,PORT=dcm_chi2,VELO=0.1,ACCL=0.2,HLM=5,LLM=-5")
dbLoadRecords("$(MOTOR)/motor.template", "P=$(PREFIX),M=dcm:z,PORT=dcm_z,VELO=1,ACCL=0.3,HLM=200,LLM=-200")

# ===== Load HFM motor records =====
dbLoadRecords("$(MOTOR)/motor.template", "P=$(PREFIX),M=hfm:pitch,PORT=hfm_pitch,VELO=0.1,ACCL=0.5,HLM=10,LLM=1")
dbLoadRecords("$(MOTOR)/motor.template", "P=$(PREFIX),M=hfm:roll,PORT=hfm_roll,VELO=0.1,ACCL=0.2,HLM=5,LLM=-5")
dbLoadRecords("$(MOTOR)/motor.template", "P=$(PREFIX),M=hfm:yaw,PORT=hfm_yaw,VELO=0.1,ACCL=0.2,HLM=5,LLM=-5")
dbLoadRecords("$(MOTOR)/motor.template", "P=$(PREFIX),M=hfm:x,PORT=hfm_x,VELO=0.2,ACCL=0.2,HLM=10,LLM=-10")
dbLoadRecords("$(MOTOR)/motor.template", "P=$(PREFIX),M=hfm:y,PORT=hfm_y,VELO=0.2,ACCL=0.2,HLM=10,LLM=-10")
dbLoadRecords("$(MOTOR)/motor.template", "P=$(PREFIX),M=hfm:z,PORT=hfm_z,VELO=1,ACCL=0.3,HLM=100,LLM=-100")
# Coddington: R = 2*p*q/(sin(α)*(p+q)), p=27m q=6m α=3mrad → R=3.27km
# MRES=1.0 to avoid int32 overflow for large radii (km scale)
dbLoadRecords("$(MOTOR)/motor.template", "P=$(PREFIX),M=hfm:rmaj,PORT=hfm_rmaj,VELO=1000000,ACCL=0.5,HLM=50000000,LLM=100000,MRES=1.0,PREC=0")
dbLoadRecords("$(MOTOR)/motor.template", "P=$(PREFIX),M=hfm:rmin,PORT=hfm_rmin,VELO=10,ACCL=0.3,HLM=1000000000,LLM=10,MRES=1.0,PREC=0")

# ===== Load VFM motor records =====
dbLoadRecords("$(MOTOR)/motor.template", "P=$(PREFIX),M=vfm:pitch,PORT=vfm_pitch,VELO=0.1,ACCL=0.5,HLM=10,LLM=1")
dbLoadRecords("$(MOTOR)/motor.template", "P=$(PREFIX),M=vfm:roll,PORT=vfm_roll,VELO=0.1,ACCL=0.2,HLM=5,LLM=-5")
dbLoadRecords("$(MOTOR)/motor.template", "P=$(PREFIX),M=vfm:yaw,PORT=vfm_yaw,VELO=0.1,ACCL=0.2,HLM=5,LLM=-5")
dbLoadRecords("$(MOTOR)/motor.template", "P=$(PREFIX),M=vfm:x,PORT=vfm_x,VELO=0.2,ACCL=0.2,HLM=10,LLM=-10")
dbLoadRecords("$(MOTOR)/motor.template", "P=$(PREFIX),M=vfm:y,PORT=vfm_y,VELO=0.2,ACCL=0.2,HLM=10,LLM=-10")
dbLoadRecords("$(MOTOR)/motor.template", "P=$(PREFIX),M=vfm:z,PORT=vfm_z,VELO=1,ACCL=0.3,HLM=100,LLM=-100")
# Coddington: R = 2*p*q/(sin(α)*(p+q)), p=30m q=3m α=3mrad → R=1.82km
dbLoadRecords("$(MOTOR)/motor.template", "P=$(PREFIX),M=vfm:rmaj,PORT=vfm_rmaj,VELO=1000000,ACCL=0.5,HLM=50000000,LLM=100000,MRES=1.0,PREC=0")
dbLoadRecords("$(MOTOR)/motor.template", "P=$(PREFIX),M=vfm:rmin,PORT=vfm_rmin,VELO=10,ACCL=0.3,HLM=1000000000,LLM=10,MRES=1.0,PREC=0")

# ===== XRT detector (AreaDetector) =====
epicsEnvSet("PREFIX", "bl:xrt:")
epicsEnvSet("PORT",       "XRT")
epicsEnvSet("QSIZE",      "20")
epicsEnvSet("XSIZE",      "$(XRT_SIZE_X)")
epicsEnvSet("YSIZE",      "$(XRT_SIZE_Y)")
epicsEnvSet("NCHANS",     "2048")
epicsEnvSet("CBUFFS",     "500")
epicsEnvSet("NELEMENTS",  "16384")
epicsEnvSet("FTVL",       "SHORT")
epicsEnvSet("TYPE",       "Int16")
epicsEnvSet("EPICS_DB_INCLUDE_PATH", "$(XRT_BEAMLINE)/db:$(ADCORE)/db")
dbLoadRecords("$(XRT_BEAMLINE)/db/xrt_detector.template", "P=$(PREFIX),R=cam1:,IOC=bl:,BL=bl:,PORT=XRT")

# Load standard areaDetector plugins
< $(ADCORE)/ioc/commonPlugins.cmd

# Restore top-level prefix
epicsEnvSet("PREFIX", "bl:")

# ===== Initial motor positions =====
# These command the motors to a starting position, so unlike the HLM/LLM
# macros above they cannot be folded into dbLoadRecords — only a running IOC
# can accept them. dbpf refuses a record whose lock set is not built yet
# (C dbTest.c:408-411, "dbpf only works after iocInit"), so the script runs
# iocInit itself here; the IocApplication build that otherwise follows the
# script finds the IOC already built.
iocInit()

# Set default positions for 8 keV operation
dbpf("bl:und:gap.VAL", "6.1")
dbpf("bl:dcm:theta.VAL", "14.31")
dbpf("bl:dcm:y.VAL", "15")
dbpf("bl:hfm:pitch.VAL", "3")
dbpf("bl:hfm:rmaj.VAL", "3272727")
dbpf("bl:vfm:pitch.VAL", "3")
dbpf("bl:vfm:rmaj.VAL", "1818182")
