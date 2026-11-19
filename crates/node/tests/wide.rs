//! 32-bit correctness: byte counters, disk sizes and times past what 32 bits
//! hold must come through whole. These run on every target; CI runs them on
//! i686 and, under qemu, on armv7, where `usize` and `c_ulong` are 32-bit.

use nightgrid_node::cli;
use nightgrid_node::metrics::collect::usage_kib;
use nightgrid_node::metrics::parse::{parse_df, parse_diskstats, parse_netdev};
use nightgrid_node::metrics::{parse, Collector, MountUsage};
use nightgrid_node::store::Store;

const FOUR_GIB: u64 = 1 << 32;

#[test]
fn network_and_disk_counters_past_2_pow_32() {
    let net = parse_netdev(&["  eth0: 5000000000 10 0 0 0 0 0 0 6000000000 10 0 0 0 0 0 0"]);
    assert_eq!((net[0].rx, net[0].tx), (5_000_000_000, 6_000_000_000));
    let disk = parse_diskstats(&["   8       0 sda 1 0 9000000000 0 1 0 9100000000 0 0 0 0"]);
    assert_eq!((disk[0].read_sectors, disk[0].write_sectors), (9_000_000_000, 9_100_000_000));

    // A rate across counters that already passed 2^32.
    let snap = |rx: u64| format!("@@netdev\n  eth0: {rx} 1 0 0 0 0 0 0 {rx} 1 0 0 0 0 0 0\n");
    let mut c = Collector::new();
    c.sample(parse(&snap(FOUR_GIB + 7)), 1_000_000.0);
    let s = c.sample(parse(&snap(FOUR_GIB + 7 + 2_000_000)), 1_002_000.0);
    assert_eq!((s.net_rx, s.net_tx), (1_000_000.0, 1_000_000.0));
}

#[test]
fn disk_sizes_past_4_gib() {
    // 12 TB in df -kP's KiB.
    let df = parse_df(&["/dev/md127 11718885376 5859442688 5859442688  50% /data"]);
    let (_, total, used, avail) = &df["/data"];
    assert_eq!((*total, *used, *avail), (11_718_885_376 * 1024, 5_859_442_688 * 1024, 5_859_442_688 * 1024));
    // statvfs block counts past 2^32 with 4 KiB blocks: 16 TiB+.
    let (t, u, a) = usage_kib(FOUR_GIB + 10, 10, 10, 4096);
    assert_eq!((t, u, a), ((FOUR_GIB + 10) * 4, FOUR_GIB * 4, 40));
    assert!(cli::bytes(t * 1024).ends_with("TiB"), "{}", cli::bytes(t * 1024));
}

#[test]
fn disk_usage_rows_keep_sizes_past_4_gib() {
    let s = Store::open_in_memory().unwrap();
    let m = MountUsage {
        device: "/dev/md127".into(),
        mount: "/data".into(),
        fstype: "btrfs".into(),
        total: 12 * FOUR_GIB * 1000,
        used: 5 * FOUR_GIB * 1000,
        avail: 7 * FOUR_GIB * 1000,
    };
    let now = 3_000_000_000; // after 2038-01-19 too.
    s.disk_usage_record(std::slice::from_ref(&m), now).unwrap();
    let rows = s.disk_usage_after(0, now + 7200, 10).unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!((rows[0].used, rows[0].size), (m.used, m.total));
    assert!(rows[0].ts > i64::from(i32::MAX));
}

#[test]
fn times_past_2038() {
    // i64 seconds everywhere; the terminal shows them even where libc's
    // time_t is 32-bit (then in UTC).
    assert_eq!(cli::utc_time(0), "1970-01-01 00:00");
    assert_eq!(cli::utc_time(i64::from(i32::MAX)), "2038-01-19 03:14");
    assert_eq!(cli::utc_time(4_102_444_800), "2100-01-01 00:00");
    assert!(
        cli::local_time(4_102_444_800).starts_with("2099-12-31")
            || cli::local_time(4_102_444_800).starts_with("2100-01-01")
    );
    assert_eq!(cli::span(i64::from(u32::MAX) + 1), "49710d 6h");
}
