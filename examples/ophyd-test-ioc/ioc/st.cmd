# ophyd test IOC — provides PVs expected by ophyd's test suite
# Replaces Docker-based epics-services-for-ophyd

# ===== Simulated Motors =====
# 6 motors matching ophyd test expectations
# simMotorCreate(port, lowLimit, highLimit, pollMs)
simMotorCreate("mtr1", -100, 100, 100)
simMotorCreate("mtr2", -100, 100, 100)
simMotorCreate("mtr3", -100, 100, 100)
simMotorCreate("mtr4", -100, 100, 100)
simMotorCreate("mtr5", -100, 100, 100)
simMotorCreate("mtr6", -100, 100, 100)

dbLoadRecords("$(MOTOR)/motor.template", "P=XF:31IDA-OP{Tbl-Ax:,M=X1}Mtr,PORT=mtr1,VELO=1.0,ACCL=0.5,MRES=0.001")
dbLoadRecords("$(MOTOR)/motor.template", "P=XF:31IDA-OP{Tbl-Ax:,M=X2}Mtr,PORT=mtr2,VELO=1.0,ACCL=0.5,MRES=0.001")
dbLoadRecords("$(MOTOR)/motor.template", "P=XF:31IDA-OP{Tbl-Ax:,M=X3}Mtr,PORT=mtr3,VELO=1.0,ACCL=0.5,MRES=0.001")
dbLoadRecords("$(MOTOR)/motor.template", "P=XF:31IDA-OP{Tbl-Ax:,M=X4}Mtr,PORT=mtr4,VELO=1.0,ACCL=0.5,MRES=0.001")
dbLoadRecords("$(MOTOR)/motor.template", "P=XF:31IDA-OP{Tbl-Ax:,M=X5}Mtr,PORT=mtr5,VELO=1.0,ACCL=0.5,MRES=0.001")
dbLoadRecords("$(MOTOR)/motor.template", "P=XF:31IDA-OP{Tbl-Ax:,M=X6}Mtr,PORT=mtr6,VELO=1.0,ACCL=0.5,MRES=0.001")

# Also load sim: prefix motors used by some tests
simMotorCreate("sim_mtr1", -100, 100, 100)
simMotorCreate("sim_mtr2", -100, 100, 100)
dbLoadRecords("$(MOTOR)/motor.template", "P=sim:,M=mtr1,PORT=sim_mtr1,VELO=1.0,ACCL=0.5,MRES=0.001")
dbLoadRecords("$(MOTOR)/motor.template", "P=sim:,M=mtr2,PORT=sim_mtr2,VELO=1.0,ACCL=0.5,MRES=0.001")

# Fake motor prefix used in tests
simMotorCreate("fake_mtr", -100, 100, 100)
dbLoadRecords("$(MOTOR)/motor.template", "P=XF:31IDA-OP{Tbl-Ax:,M=FakeMtr},PORT=fake_mtr,VELO=1.0,ACCL=0.5,MRES=0.001")

# ===== Sensors =====
# 6 sensors matching ophyd test expectations
dbLoadRecords("$(OPHYD_TEST_IOC)/db/sensor.template", "P=XF:31IDA-BI{Dev:1},R=E-I")
dbLoadRecords("$(OPHYD_TEST_IOC)/db/sensor.template", "P=XF:31IDA-BI{Dev:2},R=E-I")
dbLoadRecords("$(OPHYD_TEST_IOC)/db/sensor.template", "P=XF:31IDA-BI{Dev:3},R=E-I")
dbLoadRecords("$(OPHYD_TEST_IOC)/db/sensor.template", "P=XF:31IDA-BI{Dev:4},R=E-I")
dbLoadRecords("$(OPHYD_TEST_IOC)/db/sensor.template", "P=XF:31IDA-BI{Dev:5},R=E-I")
dbLoadRecords("$(OPHYD_TEST_IOC)/db/sensor.template", "P=XF:31IDA-BI{Dev:6},R=E-I")

# ===== SimDetector AreaDetector =====
simDetectorConfig("SIM", 640, 480, 1, 0, 50000000)

# Load SimDetector records with XF:31IDA-BI{Cam:Tbl}: prefix
epicsEnvSet("EPICS_DB_INCLUDE_PATH", "$(ADSIMDETECTOR)/db:$(ADCORE)/db")
dbLoadRecords("$(ADSIMDETECTOR)/db/simDetector.template", "P=XF:31IDA-BI{Cam:Tbl}:,R=cam1:,PORT=SIM,ADDR=0,TIMEOUT=1")

# Load standard areaDetector plugins
epicsEnvSet("PREFIX", "XF:31IDA-BI{Cam:Tbl}:")
epicsEnvSet("PORT",       "SIM")
epicsEnvSet("QSIZE",      "20")
epicsEnvSet("XSIZE",      "640")
epicsEnvSet("YSIZE",      "480")
epicsEnvSet("NCHANS",     "2048")
epicsEnvSet("CBUFFS",     "500")
epicsEnvSet("NELEMENTS",  "307200")
epicsEnvSet("FTVL",       "SHORT")
epicsEnvSet("TYPE",       "Int16")
< $(ADCORE)/ioc/commonPlugins.cmd

# Also load with ADSIM: prefix for ophyd test compatibility.
# The Configure lines fail here as in C (the ports exist); only the
# record templates load with the new prefix.
epicsEnvSet("PREFIX", "ADSIM:")
dbLoadRecords("$(ADSIMDETECTOR)/db/simDetector.template", "P=ADSIM:,R=cam1:,PORT=SIM,ADDR=0,TIMEOUT=1")
< $(ADCORE)/ioc/commonPlugins.cmd
