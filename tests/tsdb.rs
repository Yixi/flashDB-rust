//! TSDB test suite, ported from `tests/fdb_tsdb_tc.c`.
//!
//! Uses the file storage backend (file mode) like upstream, with a shared
//! monotonic clock (incrementing by `TIME_STEP` per call) so that "reboots"
//! continue the timeline, exactly as the C global `cur_times` does.

use flashdb::{FileStorage, Tsdb, TslStatus};
use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

const SECTOR_SIZE: u32 = 4096;
const TIME_STEP: i32 = 2;
const TS_COUNT: i32 = 256;

fn fresh_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("flashdb_ts_{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn make_clock(clock: &Rc<Cell<i32>>) -> impl FnMut() -> i32 + 'static {
    let c = clock.clone();
    move || {
        c.set(c.get() + TIME_STEP);
        c.get()
    }
}

fn open(dir: &Path, sectors: u32, max_len: usize, clock: &Rc<Cell<i32>>) -> Tsdb<FileStorage> {
    let storage = FileStorage::new(dir, "test_ts", SECTOR_SIZE).unwrap();
    Tsdb::new(storage, SECTOR_SIZE, SECTOR_SIZE * sectors, make_clock(clock), max_len).unwrap()
}

fn reboot(
    db: Tsdb<FileStorage>,
    dir: &Path,
    sectors: u32,
    max_len: usize,
    clock: &Rc<Cell<i32>>,
) -> Tsdb<FileStorage> {
    drop(db);
    open(dir, sectors, max_len, clock)
}

fn ralign(x: i32, a: i32) -> i32 {
    ((x + a - 1) / a) * a
}
fn ralign_down(x: i32, a: i32) -> i32 {
    (x / a) * a
}

#[test]
fn tsdb_append_iter_query_set_status() {
    let dir = fresh_dir("suite");
    let clock = Rc::new(Cell::new(0));
    let db = open(&dir, 16, 128, &clock);

    // clean
    clock.set(0);
    let mut db = reboot(db, &dir, 16, 128, &clock);
    db.clean();
    assert_eq!(db.collect().len(), 0);

    // append TS_COUNT logs; time and data string both equal the timestamp.
    for step in 1..=TS_COUNT {
        let t = step * TIME_STEP;
        let s = t.to_string();
        assert_eq!(db.append(s.as_bytes()), Ok(()));
    }

    // iter forward: tsl.time == atoi(data)
    let mut db = reboot(db, &dir, 16, 128, &clock);
    let mut count = 0;
    db.iter(|tsl, data| {
        let parsed: i32 = core::str::from_utf8(data).unwrap().parse().unwrap();
        assert_eq!(tsl.time(), parsed);
        count += 1;
        false
    });
    assert_eq!(count, TS_COUNT);

    // iter_by_time: single-timestamp queries and full-range query
    let mut db = reboot(db, &dir, 16, 128, &clock);
    let to = TS_COUNT * TIME_STEP - 1;
    for cur in (0..=to).step_by(TIME_STEP as usize) {
        let v = db.collect_by_time(cur, cur);
        if cur == 0 {
            assert!(v.is_empty());
        } else {
            assert_eq!(v.len(), 1, "cur={cur}");
            assert_eq!(v[0].time(), cur);
        }
    }
    db.iter_by_time(0, to, |tsl, data| {
        let parsed: i32 = core::str::from_utf8(data).unwrap().parse().unwrap();
        assert_eq!(tsl.time(), parsed);
        false
    });

    // query_count of WRITE records
    let mut db = reboot(db, &dir, 16, 128, &clock);
    assert_eq!(
        db.query_count(0, TS_COUNT * TIME_STEP, TslStatus::Write),
        TS_COUNT as usize
    );

    // set_status: first half -> USER_STATUS1, rest -> DELETED
    let mut db = reboot(db, &dir, 16, 128, &clock);
    let from = 0;
    let to = TS_COUNT * TIME_STEP;
    let half = (TS_COUNT / 2) * TIME_STEP;
    let tsls = db.collect_by_time(from, to);
    for tsl in &tsls {
        if tsl.time() >= 0 && tsl.time() <= half {
            db.set_status(tsl, TslStatus::UserStatus1).unwrap();
        } else {
            db.set_status(tsl, TslStatus::Deleted).unwrap();
        }
    }
    assert_eq!(
        db.query_count(from, to, TslStatus::UserStatus1),
        (TS_COUNT / 2) as usize
    );
    assert_eq!(
        db.query_count(from, to, TslStatus::Deleted),
        (TS_COUNT - TS_COUNT / 2) as usize
    );

    // clean again -> empty
    let mut db = reboot(db, &dir, 16, 128, &clock);
    db.clean();
    let mut cnt = 0;
    db.iter(|_, _| {
        cnt += 1;
        false
    });
    assert_eq!(cnt, 0);
    drop(db);
}

// ---- iter_by_time_1: rigorous time-range boundary test ----

const ITER1_COUNT: i32 = 1195; // 5 * floor((4096-31)/(13+4)), matching upstream

fn data_by_time(db: &mut Tsdb<FileStorage>, from: i32, to: i32, db_start: i32, db_end: i32) {
    let list = db.collect_by_time(from, to);
    let tsl_num = list.len() as i64;

    let (cur0, valid_to) = if from <= to {
        (
            if from < db_start { db_start } else { from },
            if to > db_end { db_end } else { to },
        )
    } else {
        (
            if from > db_end { db_end } else { from },
            if to < db_start { db_start } else { to },
        )
    };

    let mut j = 0i64;
    if from <= to {
        let mut i = cur0;
        while i <= valid_to {
            if i % TIME_STEP == 0 {
                j += 1;
            }
            i += 1;
        }
    } else {
        let mut i = cur0;
        while i >= valid_to {
            if i % TIME_STEP == 0 {
                j += 1;
            }
            i -= 1;
        }
    }
    assert_eq!(tsl_num, j, "count mismatch from={from} to={to}");

    let mut cur = cur0;
    let mut last = 0;
    for tsl in &list {
        if from <= to {
            assert_eq!(tsl.time(), ralign(cur, TIME_STEP), "fwd from={from} to={to}");
            cur += TIME_STEP;
        } else {
            assert_eq!(tsl.time(), ralign_down(cur, TIME_STEP), "rev from={from} to={to}");
            cur -= TIME_STEP;
        }
        last = tsl.time();
    }
    if tsl_num > 0 {
        if from <= to {
            assert_eq!(last, ralign_down(valid_to, TIME_STEP));
        } else {
            assert_eq!(last, ralign(valid_to, TIME_STEP));
        }
    }
}

fn get_secs_info(db: &mut Tsdb<FileStorage>) -> (i32, i32, Vec<(i32, i32)>) {
    let all = db.collect_by_time(0, i32::MAX);
    let mut secs = vec![(i32::MAX, 0i32); 10];
    let mut db_start = i32::MAX;
    let mut db_end = 0;
    for tsl in &all {
        let i = (tsl.log_addr() / SECTOR_SIZE) as usize;
        if i < secs.len() {
            if secs[i].0 > tsl.time() {
                secs[i].0 = tsl.time();
            }
            if secs[i].1 < tsl.time() {
                secs[i].1 = tsl.time();
            }
            if db_start > tsl.time() {
                db_start = tsl.time();
            }
            if db_end < tsl.time() {
                db_end = tsl.time();
            }
        }
    }
    (db_start, db_end, secs)
}

fn sector_bound(
    db: &mut Tsdb<FileStorage>,
    secs: &[(i32, i32)],
    a: usize,
    b: usize,
    ds: i32,
    de: i32,
) {
    let (a_start, a_end) = secs[a];
    let (b_start, b_end) = secs[b];
    let combos = [
        (a_start - 1, b_end + 1),
        (a_start - 1, b_end),
        (a_start - 1, b_end - 1),
        (a_start, b_end + 1),
        (a_start, b_end),
        (a_start, b_end - 1),
        (a_start + 1, b_end + 1),
        (a_start + 1, b_end),
        (a_start + 1, b_end - 1),
        (a_end - 1, b_start + 1),
        (a_end - 1, b_start),
        (a_end - 1, b_start - 1),
        (a_end, b_start + 1),
        (a_end, b_start),
        (a_end, b_start - 1),
        (a_end + 1, b_start + 1),
        (a_end + 1, b_start),
        (a_end + 1, b_start - 1),
    ];
    for (f, t) in combos {
        data_by_time(db, f, t, ds, de);
    }
}

#[test]
fn tsdb_iter_by_time_1() {
    let dir = fresh_dir("iter_by_time_1");
    let clock = Rc::new(Cell::new(0));
    let mut db = open(&dir, 16, 128, &clock);
    db.clean();

    for data in 0..ITER1_COUNT {
        db.append(&data.to_le_bytes()).unwrap();
    }

    let mut db = reboot(db, &dir, 16, 128, &clock);

    let (db_start, db_end, secs) = get_secs_info(&mut db);
    // must span more than 2 sectors
    assert_ne!(secs[2].0, i32::MAX);

    // database bounds
    data_by_time(&mut db, db_start - 1, db_end + 1, db_start, db_end);
    data_by_time(&mut db, db_start - 2, db_start - 1, db_start, db_end);
    data_by_time(&mut db, db_start - 1, db_start - 2, db_start, db_end);
    data_by_time(&mut db, db_end + 1, db_end + 2, db_start, db_end);
    data_by_time(&mut db, db_end + 2, db_end + 1, db_start, db_end);

    // first sector
    data_by_time(&mut db, secs[0].0 - 1, secs[0].1, db_start, db_end);
    data_by_time(&mut db, secs[0].0, secs[0].1, db_start, db_end);
    data_by_time(&mut db, secs[0].0, secs[0].1 + 1, db_start, db_end);
    data_by_time(&mut db, secs[0].1 + 1, secs[0].0, db_start, db_end);
    data_by_time(&mut db, secs[0].1, secs[0].0, db_start, db_end);
    data_by_time(&mut db, secs[0].1, secs[0].0 - 1, db_start, db_end);

    // last used sector
    let mut last_idx = 0;
    for (i, s) in secs.iter().enumerate() {
        if s.1 == 0 {
            last_idx = i;
            break;
        }
    }
    assert!(last_idx >= 3);
    let last = secs[last_idx - 1];
    data_by_time(&mut db, last.0 - 1, last.1, db_start, db_end);
    data_by_time(&mut db, last.0, last.1, db_start, db_end);
    data_by_time(&mut db, last.0, last.1 + 1, db_start, db_end);
    data_by_time(&mut db, last.1 + 1, last.0, db_start, db_end);
    data_by_time(&mut db, last.1, last.0, db_start, db_end);
    data_by_time(&mut db, last.1, last.0 - 1, db_start, db_end);

    // less than / equal to one sector
    data_by_time(&mut db, secs[0].0 + 1, secs[0].1 - 1, db_start, db_end);
    data_by_time(&mut db, secs[0].1 - 1, secs[0].0 + 1, db_start, db_end);
    data_by_time(&mut db, secs[0].0, secs[0].1, db_start, db_end);
    data_by_time(&mut db, secs[0].1, secs[0].0, db_start, db_end);

    // 1~2 sector combinations
    sector_bound(&mut db, &secs, 0, 0, db_start, db_end);
    sector_bound(&mut db, &secs, 0, 1, db_start, db_end);
    sector_bound(&mut db, &secs, 1, 0, db_start, db_end);
    sector_bound(&mut db, &secs, 1, 1, db_start, db_end);

    // more than 2 sectors
    sector_bound(&mut db, &secs, 0, 2, db_start, db_end);
    sector_bound(&mut db, &secs, 2, 0, db_start, db_end);
    sector_bound(&mut db, &secs, 2, 2, db_start, db_end);
    drop(db);
}

#[test]
fn tsdb_github_issue_249() {
    let dir = fresh_dir("issue_249");
    let clock = Rc::new(Cell::new(0));
    let sec = 16 * 1024u32;
    let max = 512 * 1024u32;
    let max_len = 10 * 1024usize;

    let storage = FileStorage::new(&dir, "storage_tsdb", sec).unwrap();
    let mut db = Tsdb::new(storage, sec, max, make_clock(&clock), max_len).unwrap();
    db.clean();
    clock.set(0);

    for size in [7 * 1024usize, 8 * 1024, 9 * 1024] {
        let data = vec![0xABu8; size];
        assert_eq!(db.append(&data), Ok(()));
    }

    // reboot
    drop(db);
    let storage = FileStorage::new(&dir, "storage_tsdb", sec).unwrap();
    let mut db = Tsdb::new(storage, sec, max, make_clock(&clock), max_len).unwrap();

    assert_eq!(db.query_count(2, 6, TslStatus::Write), 3);
    assert_eq!(db.query_count(0, i32::MAX, TslStatus::Write), 3);
    drop(db);
}
