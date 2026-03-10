#!../../target/debug/sim_ioc
#============================================================
# st.cmd — SimDetector IOC startup script
#
# This is an iocsh script executed by sim_ioc, matching the
# C++ ADSimDetector IOC startup structure.
#
# Usage:
#   cargo run --bin sim_ioc --features ioc -- ioc/st.cmd
#============================================================

# Environment
epicsEnvSet("PREFIX", "SIM1:")
epicsEnvSet("CAM",    "cam1:")
epicsEnvSet("XSIZE",  "1024")
epicsEnvSet("YSIZE",  "1024")
epicsEnvSet("NCHANS", "2048")
epicsEnvSet("EPICS_DB_INCLUDE_PATH", "$(ADCORE)/ADApp/Db")

# Create the SimDetector driver
# simDetectorConfig(portName, sizeX, sizeY, maxMemory)
simDetectorConfig("SIM1", 256, 256, 50000000)

# Load the detector database from .template (includes ADBase + NDArrayBase)
dbLoadRecords("$(ADSIMDETECTOR)/simDetectorApp/Db/simDetector.template", "P=$(PREFIX),R=$(CAM),PORT=SIM1,DTYP=asynSimDetector")

# ===== Plugins =====

# StdArrays plugin (image data for clients)
NDStdArraysConfigure("IMAGE1", "asynStdArrays")
dbLoadRecords("$(ADCORE)/ADApp/Db/NDStdArrays.template", "P=$(PREFIX),R=image1:,PORT=IMAGE1,DTYP=asynStdArrays,NDARRAY_PORT=SIM1,FTVL=UCHAR,NELEMENTS=65536")

# Stats plugin (min/max/mean/sigma/centroid)
NDStatsConfigure("STATS1", "asynStats1", 10)
dbLoadRecords("$(ADCORE)/ADApp/Db/NDStats.template", "P=$(PREFIX),R=Stats1:,PORT=STATS1,DTYP=asynStats1,NCHANS=$(NCHANS),XSIZE=$(XSIZE),YSIZE=$(YSIZE),HIST_SIZE=256")

# ROI plugin
NDROIConfigure("ROI1", "asynROI1", 10)
dbLoadRecords("$(ADCORE)/ADApp/Db/NDPluginBase.template", "P=$(PREFIX),R=ROI1:,DTYP=asynROI1,NDARRAY_PORT=SIM1")

# Process plugin
NDProcessConfigure("PROC1", "asynProc1", 10)
dbLoadRecords("$(ADCORE)/ADApp/Db/NDPluginBase.template", "P=$(PREFIX),R=Proc1:,DTYP=asynProc1,NDARRAY_PORT=SIM1")

# iocInit is called automatically by IocApplication after this script completes.
#
# After init, the interactive iocsh shell starts.
#
# Example interactive commands:
#   dbl                                # List all PVs
#   dbpf SIM1:cam1:Acquire 1           # Start acquisition
#   dbgf SIM1:cam1:ArrayCounter_RBV    # Read frame counter
#   dbpf SIM1:cam1:SimMode 1           # Switch to Peaks mode
#   dbpf SIM1:cam1:Acquire 0           # Stop acquisition
#   simDetectorReport                  # Show detector status
