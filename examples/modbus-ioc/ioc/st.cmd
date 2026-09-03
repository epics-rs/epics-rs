#============================================================
# st.cmd — Modbus IOC startup script
#
# Usage:
#   cargo run --release -p modbus-ioc --bin modbus_ioc --features ioc -- ioc/st.cmd
#
# Requires a reachable Modbus/TCP server. To run a local simulator:
#   pip install pymodbus && pymodbus.simulator   # or: diagslave -m tcp
#============================================================

epicsEnvSet("P",        "TEST:")
epicsEnvSet("R",        "MB:")
epicsEnvSet("OCTET",    "MBTCP1")
epicsEnvSet("HOST",     "127.0.0.1:502")

# ---- underlying IP octet port ----
drvAsynIPPortConfigure("$(OCTET)", "$(HOST)")

# ---- Modbus link framing: 0=TCP 1=RTU 2=ASCII 3=UDP ----
modbusInterposeConfig("$(OCTET)", 0, 1000, 0)

# ---- Modbus driver ports ----
# drvModbusAsynConfigure(port, octetPort, slave, function,
#                        startAddr, length, dataType, pollMsec, plcType)
#
# Read 10 holding registers (function 3) from address 0, poll every 100 ms.
drvModbusAsynConfigure("$(R)HR", "$(OCTET)", 0, 3, 0, 10, "UINT16", 100, "")
#
# Write 10 holding registers (function 16) at address 100.
drvModbusAsynConfigure("$(R)HW", "$(OCTET)", 0, 16, 100, 10, "UINT16", 0, "")

dbLoadRecords("$(MODBUS_IOC)/db/modbus.db", "P=$(P),R=$(R),HR=$(R)HR,HW=$(R)HW")

iocInit()

# Example:
#   dbl
#   camonitor TEST:MB:Reg0 TEST:MB:Reg1
#   caput TEST:MB:SetReg0 1234
#   caget TEST:MB:HR:ReadOK TEST:MB:HR:IOErrors
