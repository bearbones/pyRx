import pytest

@pytest.fixture
def connection():
    return helper()

def helper():
    return 42

def async_setup_entry():
    return False
