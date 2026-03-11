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
simMotorCreate("dot_mtrx", -100, 100, 100)
simMotorCreate("dot_mtry", -100, 100, 100)

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
dbLoadRecords("$(MOTOR)/motor.template", "P=$(PREFIX),M=dot:mtrx,PORT=dot_mtrx")
dbLoadRecords("$(MOTOR)/motor.template", "P=$(PREFIX),M=dot:mtry,PORT=dot_mtry")

# Load beam current
dbLoadRecords("$(MINI_BEAMLINE)/db/beam_current.template", "P=$(PREFIX)")

# Load point detectors
dbLoadRecords("$(MINI_BEAMLINE)/db/point_detector.template", "P=$(PREFIX),R=ph:,MTR=ph:mtr,DTYP=asynPointDet_PH")
dbLoadRecords("$(MINI_BEAMLINE)/db/point_detector.template", "P=$(PREFIX),R=edge:,MTR=edge:mtr,DTYP=asynPointDet_EDGE")
dbLoadRecords("$(MINI_BEAMLINE)/db/point_detector.template", "P=$(PREFIX),R=slit:,MTR=slit:mtr,DTYP=asynPointDet_SLIT")

# Load moving dot detector
dbLoadRecords("$(MINI_BEAMLINE)/db/moving_dot.template", "P=$(PREFIX),R=dot:cam:,PORT=DOT,DTYP=asynMovingDot")

# Load standard areaDetector plugins for MovingDot
< $(ADCORE)/ioc/commonPlugins.cmd
