-- 0003 (N3): devices seen on the LAN, from the kernel's neighbour table.
-- One row per MAC address with its latest IPv4 address. Rows not seen for
-- 400 days are dropped.
CREATE TABLE net_devices (
    mac        TEXT PRIMARY KEY,
    ip         TEXT NOT NULL,
    iface      TEXT NOT NULL,
    first_seen INTEGER NOT NULL,
    last_seen  INTEGER NOT NULL
) STRICT;

CREATE INDEX net_devices_ip ON net_devices(ip);
