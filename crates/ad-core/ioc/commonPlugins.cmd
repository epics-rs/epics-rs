# commonPlugins.cmd — Common plugin configuration for areaDetector IOCs
#
# Mirrors the C EPICS ADCore/iocBoot/EXAMPLE_commonPlugins.cmd structure.
# Loaded from st.cmd via: < commonPlugins.cmd
#
# Required macros (set before loading):
#   $(PREFIX)  - PV prefix
#   $(PORT)    - Detector port name
#   $(QSIZE)   - Queue size (default 20)
#   $(XSIZE)   - Max image width
#   $(YSIZE)   - Max image height
#   $(NCHANS)  - Max time series points
#   $(CBUFFS)  - Circular buffer frame count (default 500)

# ===== File saving plugins =====

NDFileNetCDFConfigure("FileNetCDF1", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDPluginBase.template", "P=$(PREFIX),R=netCDF1:,PORT=FileNetCDF1,DTYP=asynFileNetCDF1,NDARRAY_PORT=$(PORT)")

NDFileTIFFConfigure("FileTIFF1", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDPluginBase.template", "P=$(PREFIX),R=TIFF1:,PORT=FileTIFF1,DTYP=asynFileTIFF1,NDARRAY_PORT=$(PORT)")

NDFileJPEGConfigure("FileJPEG1", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDPluginBase.template", "P=$(PREFIX),R=JPEG1:,PORT=FileJPEG1,DTYP=asynFileJPEG1,NDARRAY_PORT=$(PORT)")

NDFileNexusConfigure("FileNexus1", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDPluginBase.template", "P=$(PREFIX),R=Nexus1:,PORT=FileNexus1,DTYP=asynFileNexus1,NDARRAY_PORT=$(PORT)")

NDFileHDF5Configure("FileHDF1", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDPluginBase.template", "P=$(PREFIX),R=HDF1:,PORT=FileHDF1,DTYP=asynFileHDF1,NDARRAY_PORT=$(PORT)")

#NDFileMagickConfigure("FileMagick1", $(QSIZE), 0, "$(PORT)", 0)
#dbLoadRecords("NDPluginBase.template", "P=$(PREFIX),R=Magick1:,PORT=FileMagick1,DTYP=asynFileMagick1,NDARRAY_PORT=$(PORT)")

# ===== ROI plugins (4 instances) =====

NDROIConfigure("ROI1", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDPluginBase.template", "P=$(PREFIX),R=ROI1:,PORT=ROI1,DTYP=asynROI1,NDARRAY_PORT=$(PORT)")

NDROIConfigure("ROI2", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDPluginBase.template", "P=$(PREFIX),R=ROI2:,PORT=ROI2,DTYP=asynROI2,NDARRAY_PORT=$(PORT)")

NDROIConfigure("ROI3", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDPluginBase.template", "P=$(PREFIX),R=ROI3:,PORT=ROI3,DTYP=asynROI3,NDARRAY_PORT=$(PORT)")

NDROIConfigure("ROI4", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDPluginBase.template", "P=$(PREFIX),R=ROI4:,PORT=ROI4,DTYP=asynROI4,NDARRAY_PORT=$(PORT)")

# ===== ROI statistics =====

NDROIStatConfigure("ROISTAT1", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDPluginBase.template", "P=$(PREFIX),R=ROIStat1:,PORT=ROISTAT1,DTYP=asynROISTAT1,NDARRAY_PORT=$(PORT)")

# ===== Processing plugin =====

NDProcessConfigure("PROC1", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDPluginBase.template", "P=$(PREFIX),R=Proc1:,PORT=PROC1,DTYP=asynPROC1,NDARRAY_PORT=$(PORT)")

# ===== Scatter/Gather =====

NDScatterConfigure("SCATTER1", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDPluginBase.template", "P=$(PREFIX),R=Scatter1:,PORT=SCATTER1,DTYP=asynSCATTER1,NDARRAY_PORT=$(PORT)")

NDGatherConfigure("GATHER1", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDPluginBase.template", "P=$(PREFIX),R=Gather1:,PORT=GATHER1,DTYP=asynGATHER1,NDARRAY_PORT=$(PORT)")

# ===== Statistics plugins (5 instances) =====

NDStatsConfigure("STATS1", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDStats.template", "P=$(PREFIX),R=Stats1:,PORT=STATS1,DTYP=asynSTATS1,NCHANS=$(NCHANS),XSIZE=$(XSIZE),YSIZE=$(YSIZE),HIST_SIZE=256,NDARRAY_PORT=$(PORT)")

NDStatsConfigure("STATS2", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDStats.template", "P=$(PREFIX),R=Stats2:,PORT=STATS2,DTYP=asynSTATS2,NCHANS=$(NCHANS),XSIZE=$(XSIZE),YSIZE=$(YSIZE),HIST_SIZE=256,NDARRAY_PORT=$(PORT)")

NDStatsConfigure("STATS3", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDStats.template", "P=$(PREFIX),R=Stats3:,PORT=STATS3,DTYP=asynSTATS3,NCHANS=$(NCHANS),XSIZE=$(XSIZE),YSIZE=$(YSIZE),HIST_SIZE=256,NDARRAY_PORT=$(PORT)")

NDStatsConfigure("STATS4", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDStats.template", "P=$(PREFIX),R=Stats4:,PORT=STATS4,DTYP=asynSTATS4,NCHANS=$(NCHANS),XSIZE=$(XSIZE),YSIZE=$(YSIZE),HIST_SIZE=256,NDARRAY_PORT=$(PORT)")

NDStatsConfigure("STATS5", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDStats.template", "P=$(PREFIX),R=Stats5:,PORT=STATS5,DTYP=asynSTATS5,NCHANS=$(NCHANS),XSIZE=$(XSIZE),YSIZE=$(YSIZE),HIST_SIZE=256,NDARRAY_PORT=$(PORT)")

# ===== Transform plugin =====

NDTransformConfigure("TRANS1", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDPluginBase.template", "P=$(PREFIX),R=Trans1:,PORT=TRANS1,DTYP=asynTRANS1,NDARRAY_PORT=$(PORT)")

# ===== Overlay plugin =====

NDOverlayConfigure("OVER1", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDPluginBase.template", "P=$(PREFIX),R=Over1:,PORT=OVER1,DTYP=asynOVER1,NDARRAY_PORT=$(PORT)")

# ===== Color conversion plugins (2 instances) =====

NDColorConvertConfigure("CC1", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDPluginBase.template", "P=$(PREFIX),R=CC1:,PORT=CC1,DTYP=asynCC1,NDARRAY_PORT=$(PORT)")

NDColorConvertConfigure("CC2", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDPluginBase.template", "P=$(PREFIX),R=CC2:,PORT=CC2,DTYP=asynCC2,NDARRAY_PORT=$(PORT)")

# ===== Circular buffer =====

NDCircularBuffConfigure("CB1", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDPluginBase.template", "P=$(PREFIX),R=CB1:,PORT=CB1,DTYP=asynCB1,NDARRAY_PORT=$(PORT)")

# ===== Attributes =====

NDAttrConfigure("ATTR1", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDPluginBase.template", "P=$(PREFIX),R=Attr1:,PORT=ATTR1,DTYP=asynATTR1,NDARRAY_PORT=$(PORT)")

# ===== FFT =====

NDFFTConfigure("FFT1", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDPluginBase.template", "P=$(PREFIX),R=FFT1:,PORT=FFT1,DTYP=asynFFT1,NDARRAY_PORT=$(PORT)")

# ===== Codec plugins (2 instances) =====

NDCodecConfigure("CODEC1", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDPluginBase.template", "P=$(PREFIX),R=Codec1:,PORT=CODEC1,DTYP=asynCODEC1,NDARRAY_PORT=$(PORT)")

NDCodecConfigure("CODEC2", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDPluginBase.template", "P=$(PREFIX),R=Codec2:,PORT=CODEC2,DTYP=asynCODEC2,NDARRAY_PORT=$(PORT)")

# ===== Bad pixel =====

NDBadPixelConfigure("BADPIX1", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDPluginBase.template", "P=$(PREFIX),R=BadPix1:,PORT=BADPIX1,DTYP=asynBADPIX1,NDARRAY_PORT=$(PORT)")
