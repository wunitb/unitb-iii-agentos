# ปิดรายการค้าง — cargo-deny และ native OTEL — 2026-09-06

## ขอบเขตที่ปิด

ปิดสองรายการค้างจากรอบ Node dependency migration: full `cargo-deny` และการพิสูจน์ OpenTelemetry กับ iii engine จริง. ผลตรวจทรีรอบสุดท้ายและ source hashes อยู่ใน [closure evidence](evidence/closure-2026-09-06.json). JSON ของรอบก่อนเก็บเป็นประวัติ ไม่แก้ผล FAIL เดิมย้อนหลัง.

### Rust dependency gate

Pulse ใช้ `cron@0.12.1` เพื่อถามเพียงว่า UTC timestamp อยู่ใน due slot หรือไม่ ไม่ได้ใช้สร้างลำดับงานอนาคต; engine เป็นผู้สร้าง cron triggers. Dependency นี้ทำให้ shipped-target graph มี `nom` 7 และ 8 พร้อมกัน.

`workers/pulse/src/schedule.rs` จึงเป็น private membership matcher แทน dependency ดังกล่าว. แต่ละ field เก็บเป็น fixed-size bit mask; ไม่มี future-schedule iterator หรือ dependency ใหม่. คง normalization ห้า/หก fields, Sunday=1, day-of-month AND weekday, lists/ranges/steps, ชื่อเดือนและวัน, `?` เฉพาะ day fields และขอบเขตปีเดิม. ไม่เปลี่ยน jitter window, principal checks, in-flight guard หรือ due-slot deduplication.

ก่อนถอด crate ได้รันเทียบกับ `cron@0.12.1` จริง: corpus 176 expressions, 97 expressions ที่ parser เดิมยอมรับ, ตรวจ exact field ordinals และ 4,392 UTC timestamps ต่อ accepted expression. เก็บผลอ้างอิงเป็น hexadecimal masks ใน `workers/pulse/tests/cron-corpus.json` เพื่อไม่เสีย precision เมื่ออ่าน JSON. Permanent test ตรวจ corpus นี้หลังนำ dependency เก่าออกแล้ว และมี calendar/intersection/bounds tests แยก.

`deny.toml` และเกณฑ์ exception ไม่เปลี่ยน; ไม่เพิ่ม package หรือ exception เพื่อทำให้ gate เขียว. `nom` 7 อาจยังอยู่ใน lockfile สำหรับ dependencies นอก shipped-target graph; ไม่อ้างว่าทุก platform ใน lockfile ไม่มี duplicate.

### Native-engine OTEL ingestion

`bun run test:otel:native` ใช้ `scripts/native-otel.mjs` เปิด iii **0.22.1** ใน scratch runtime แยกจาก source tree. มีเฉพาะ worker manager, observability และ configuration สำหรับการทดสอบนี้; ปิด external builtin daemons และ anonymous telemetry. ไม่ใช้ provider/channel credentials.

ทดสอบ Node และ Bun ทั้ง ESM/CommonJS ด้วย pinned SDK และ compatibility patch จริง. แต่ละ process register echo handler แล้วเรียกผ่าน engine จากนั้น flush และ query-back จาก `engine::traces::list`, `engine::logs::list`, `engine::metrics::list`. ตรวจ trace/span IDs, correlated log, resource/service identity, baggage และ metric value ไม่ใช้แค่การเปิด socket หรือ mock receiver เป็นหลักฐาน.

ใช้ portless เมื่อมี CLI; CI ที่ไม่มี portless ให้ kernel จัดสรร test port. ตรวจ engine exit, listener closure และลบ scratch หลังหยุด. CI เพิ่ม required step หลังติดตั้ง pinned iii ก่อนเริ่ม full-stack fake-provider lane. Native receiver เป็น engine จริง แต่ไม่ใช่การเปิด AgentOS ทุก worker หรือการรับรอง external provider.

## ตรวจซ้ำจาก repository root

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --offline --locked
cargo build --workspace --release --offline --locked
cargo deny check
cargo audit
bun install --frozen-lockfile
bun run check
bun run audit
bun run test:otel:native
```

ใช้ Rust 1.90, Bun 1.3.14 และ cargo-deny 0.20.2 ตาม pin ของ repository/CI. Fresh advisory checks ต้องใช้ network; offline Cargo commands ต้องมี dependencies ใน cache. Native test ต้องมี pinned iii และ Node/Bun binaries บน PATH.

การปิดสองรายการนี้ไม่ใช่ permission ให้ commit/push/merge/release และไม่ใช่ public CI/attestation หรือ real-provider acceptance. งานนี้ไม่เปลี่ยน repository security settings, credentials, operator data หรือ historical branches/worktrees.
