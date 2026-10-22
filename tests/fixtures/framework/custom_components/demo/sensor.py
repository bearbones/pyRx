from homeassistant.components.sensor import SensorEntity

class DemoSensor(SensorEntity):
    @property
    def native_value(self):
        return 42
