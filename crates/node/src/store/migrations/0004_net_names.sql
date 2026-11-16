-- 0004 (N3b): the host name a device asked DHCP for, when the network
-- watch helper saw it ('' when unknown).
ALTER TABLE net_devices ADD COLUMN name TEXT NOT NULL DEFAULT '';
