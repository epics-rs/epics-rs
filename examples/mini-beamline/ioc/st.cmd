epicsEnvSet("PREFIX", "mini:")

# ===== Beam current parameters =====
epicsEnvSet("BEAM_OFFSET",    "500.0")
epicsEnvSet("BEAM_AMPLITUDE", "25.0")
epicsEnvSet("BEAM_PERIOD",    "4.0")
epicsEnvSet("BEAM_UPDATE_MS", "100")

# ===== Simulated Motors =====
# simMotorCreate(port, lowLimit, highLimit, pollMs)
simMotorCreate("ph_mtr", -100, 100, 100)
simMotorCreate("edge_mtr", -100, 100, 100)
simMotorCreate("slit_mtr", -100, 100, 100)
simMotorCreate("dot_mtrx", -500, 500, 100)
simMotorCreate("dot_mtry", -500, 500, 100)

# ===== Kohzu DCM (Double Crystal Monochromator) =====
simMotorCreate("dcm_theta", -10, 90, 100)
simMotorCreate("dcm_y", -50, 50, 100)
simMotorCreate("dcm_z", -50, 50, 100)

# ===== Simulated HSC-1 Slit Controller =====
simHscCreate("HSC1", 100)

# ===== Simulated Quad BPM =====
simQxbpmCreate("QXBPM1", 0.0, 0.0, 100)

# ===== MovingDot detector parameters =====
epicsEnvSet("DOT_SIZE_X",       "640")
epicsEnvSet("DOT_SIZE_Y",       "480")
epicsEnvSet("DOT_MAX_MEMORY",   "50000000")
epicsEnvSet("DOT_SIGMA_X",      "50.0")
epicsEnvSet("DOT_SIGMA_Y",      "25.0")
epicsEnvSet("DOT_BACKGROUND",   "1000.0")
epicsEnvSet("DOT_N_PER_I_PER_S","200.0")

# Configure all beamline components
miniBeamlineConfig()

# Load motors
dbLoadRecords("$(MOTOR)/motor.template", "P=$(PREFIX),M=ph:mtr,PORT=ph_mtr")
dbLoadRecords("$(MOTOR)/motor.template", "P=$(PREFIX),M=edge:mtr,PORT=edge_mtr")
dbLoadRecords("$(MOTOR)/motor.template", "P=$(PREFIX),M=slit:mtr,PORT=slit_mtr")
dbLoadRecords("$(MOTOR)/motor.template", "P=$(PREFIX),M=dot:mtrx,PORT=dot_mtrx,VELO=500,ACCL=0.1,HLM=500,LLM=-500")
dbLoadRecords("$(MOTOR)/motor.template", "P=$(PREFIX),M=dot:mtry,PORT=dot_mtry,VELO=500,ACCL=0.1,HLM=500,LLM=-500")

# Load DCM motors and sequencer database
dbLoadRecords("$(MOTOR)/motor.template", "P=$(PREFIX),M=dcm:theta,PORT=dcm_theta")
dbLoadRecords("$(MOTOR)/motor.template", "P=$(PREFIX),M=dcm:y,PORT=dcm_y")
dbLoadRecords("$(MOTOR)/motor.template", "P=$(PREFIX),M=dcm:z,PORT=dcm_z")
# Widen DCM Z soft limits for full energy range (5-20 keV needs Z up to ~200mm)
dbpf("$(PREFIX)dcm:z.DHLM", "250")
dbpf("$(PREFIX)dcm:z.DLLM", "-250")
dbpf("$(PREFIX)dcm:y.DHLM", "250")
dbpf("$(PREFIX)dcm:y.DLLM", "-250")
dbLoadRecords("$(OPTICS)/db/kohzuSeq.db", "P=$(PREFIX),M_THETA=dcm:theta,M_Y=dcm:y,M_Z=dcm:z,yOffHi=50,yOffLo=-50")

# Load beam current
dbLoadRecords("$(MINI_BEAMLINE)/db/beam_current.template", "P=$(PREFIX)")

# Random-waveform PVs for bulk-transfer benchmarks
# (mini:wf1..mini:wf10, each a 10000-element f64 waveform)
dbLoadRecords("$(MINI_BEAMLINE)/db/random_waveform.template", "P=$(PREFIX)wf,N=1,NELM=10000")
dbLoadRecords("$(MINI_BEAMLINE)/db/random_waveform.template", "P=$(PREFIX)wf,N=2,NELM=10000")
dbLoadRecords("$(MINI_BEAMLINE)/db/random_waveform.template", "P=$(PREFIX)wf,N=3,NELM=10000")
dbLoadRecords("$(MINI_BEAMLINE)/db/random_waveform.template", "P=$(PREFIX)wf,N=4,NELM=10000")
dbLoadRecords("$(MINI_BEAMLINE)/db/random_waveform.template", "P=$(PREFIX)wf,N=5,NELM=10000")
dbLoadRecords("$(MINI_BEAMLINE)/db/random_waveform.template", "P=$(PREFIX)wf,N=6,NELM=10000")
dbLoadRecords("$(MINI_BEAMLINE)/db/random_waveform.template", "P=$(PREFIX)wf,N=7,NELM=10000")
dbLoadRecords("$(MINI_BEAMLINE)/db/random_waveform.template", "P=$(PREFIX)wf,N=8,NELM=10000")
dbLoadRecords("$(MINI_BEAMLINE)/db/random_waveform.template", "P=$(PREFIX)wf,N=9,NELM=10000")
dbLoadRecords("$(MINI_BEAMLINE)/db/random_waveform.template", "P=$(PREFIX)wf,N=10,NELM=10000")

# Load point detectors
dbLoadRecords("$(MINI_BEAMLINE)/db/point_detector.template", "P=$(PREFIX),R=ph:,MTR=ph:mtr,PORT=PD_PH")
dbLoadRecords("$(MINI_BEAMLINE)/db/point_detector.template", "P=$(PREFIX),R=edge:,MTR=edge:mtr,PORT=PD_EDGE")
dbLoadRecords("$(MINI_BEAMLINE)/db/point_detector.template", "P=$(PREFIX),R=slit:,MTR=slit:mtr,PORT=PD_SLIT")

# ===== MovingDot area detector (AD convention: P=detector prefix, R=cam1:) =====
epicsEnvSet("PREFIX", "mini:dot:")
epicsEnvSet("PORT",       "DOT")
epicsEnvSet("QSIZE",      "20")
epicsEnvSet("XSIZE",      "$(DOT_SIZE_X)")
epicsEnvSet("YSIZE",      "$(DOT_SIZE_Y)")
epicsEnvSet("NCHANS",     "2048")
epicsEnvSet("CBUFFS",     "500")
epicsEnvSet("NELEMENTS",  "307200")
epicsEnvSet("FTVL",       "SHORT")
epicsEnvSet("TYPE",       "Int16")
epicsEnvSet("EPICS_DB_INCLUDE_PATH", "$(MINI_BEAMLINE)/db:$(ADCORE)/db")
dbLoadRecords("$(MINI_BEAMLINE)/db/moving_dot.template", "P=$(PREFIX),R=cam1:,IOC=mini:,PORT=DOT")

# Load standard areaDetector plugins for MovingDot
< $(ADCORE)/ioc/commonPlugins.cmd

# Restore top-level prefix
epicsEnvSet("PREFIX", "mini:")

# ===== State machines (spawned here, wait internally for iocInit + PV availability) =====
seqStart("kohzuCtl", "P=$(PREFIX),M_THETA=dcm:theta,M_Y=dcm:y,M_Z=dcm:z")

# ===== Autosave =====
set_savefile_path("$(MINI_BEAMLINE)/ioc/autosave")
set_requestfile_path("$(MINI_BEAMLINE)/ioc/autosave")
set_requestfile_path("$(MINI_BEAMLINE)/db")
set_requestfile_path("$(ADCORE)/db")
set_pass0_restoreFile("mini_positions.sav")
set_pass0_restoreFile("moving_dot_settings.req", "P=mini:dot:,R=cam1:")
set_pass1_restoreFile("moving_dot_settings.req", "P=mini:dot:,R=cam1:")
create_monitor_set("mini_positions.req", 5)
create_monitor_set("moving_dot_settings.req", 5, "P=mini:dot:,R=cam1:")
