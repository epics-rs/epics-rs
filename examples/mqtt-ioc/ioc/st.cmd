#============================================================
# st.cmd — MQTT IOC startup script (Z2M builder version)
#
# Usage:
#   cargo run --release -p mqtt-ioc --bin mqtt-ioc -- ioc/st.cmd
#
# Each mqttZ2m* command registers topics AND creates records.
# No .db file needed for Z2M devices.
#============================================================

epicsEnvSet("PREFIX", "TEST:")
epicsEnvSet("R",      "MQTT:")
epicsEnvSet("PORT",   "MQTT1")
epicsEnvSet("BROKER", "mqtt://localhost:1883")

# ===================== Smart Plugs =====================
mqttZ2mPlug("$(PORT)", "$(PREFIX)$(R)", "LR:Plug",  "zigbee2mqtt/living room plug")
mqttZ2mPlug("$(PORT)", "$(PREFIX)$(R)", "BR1:Plug", "zigbee2mqtt/bedroom 1 plug")

# ===================== Lights =====================
mqttZ2mLight("$(PORT)", "$(PREFIX)$(R)", "BR1:Desk", "zigbee2mqtt/desk light")

# ===================== Switch Modules =====================
mqttZ2mSwitch("$(PORT)", "$(PREFIX)$(R)", "Bath1:Light", "zigbee2mqtt/bathroom 1 light")
mqttZ2mSwitch("$(PORT)", "$(PREFIX)$(R)", "Bath1:Fan",   "zigbee2mqtt/bathroom 1 fan")
mqttZ2mSwitch("$(PORT)", "$(PREFIX)$(R)", "Bath2:Light", "zigbee2mqtt/bathroom 2 light")
mqttZ2mSwitch("$(PORT)", "$(PREFIX)$(R)", "Bath2:Fan",   "zigbee2mqtt/bathroom 2 fan")

# ===================== Temp/Humidity Sensors =====================
mqttZ2mTempSensor("$(PORT)", "$(PREFIX)$(R)", "BR1:Sens",   "zigbee2mqtt/bedroom 1 sensor")
mqttZ2mTempSensor("$(PORT)", "$(PREFIX)$(R)", "LR:Sens",    "zigbee2mqtt/living room sensor")
mqttZ2mTempSensor("$(PORT)", "$(PREFIX)$(R)", "BR2:Sens",   "zigbee2mqtt/bedroom 2 sensor")
mqttZ2mTempSensor("$(PORT)", "$(PREFIX)$(R)", "BR3:Sens",   "zigbee2mqtt/bedroom 3 sensor")
mqttZ2mTempSensor("$(PORT)", "$(PREFIX)$(R)", "LDRY:Sens",  "zigbee2mqtt/laundry sensor")
mqttZ2mTempSensor("$(PORT)", "$(PREFIX)$(R)", "Bath1:Sens", "zigbee2mqtt/bathroom 1 sensor")
mqttZ2mTempSensor("$(PORT)", "$(PREFIX)$(R)", "Bath2:Sens", "zigbee2mqtt/bathroom 2 sensor")

# ===================== Motion Sensor =====================
mqttZ2mMotion("$(PORT)", "$(PREFIX)$(R)", "ENT:Motion", "zigbee2mqtt/entrance motion")

# ===================== Remote Buttons =====================
mqttZ2mRemote2("$(PORT)", "$(PREFIX)$(R)", "BR1:Desk:Btn", "zigbee2mqtt/desk light button")
mqttZ2mRemote2("$(PORT)", "$(PREFIX)$(R)", "Bath2:Sw",     "zigbee2mqtt/bathroom 2 switch")
mqttZ2mRemote2("$(PORT)", "$(PREFIX)$(R)", "Bath1:Sw",     "zigbee2mqtt/bathroom 1 switch")

# ===================== Driver =====================
mqttDriverConfigure("$(PORT)", "$(BROKER)", "epics-mqtt-ioc", 1, "$(PREFIX)$(R)Connected")

iocInit()

# Example:
#   dbl
#   camonitor TEST:MQTT:LR:Plug:Power TEST:MQTT:LR:Sens:Temp
#   caget TEST:MQTT:Connected
#   caput TEST:MQTT:LR:Plug:SetState ON
#   caput TEST:MQTT:Bath1:Light:SetState ON
#   caput TEST:MQTT:BR1:Desk:SetBright 128
