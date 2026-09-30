use std::error::Error;

use ferro_db::config::PAGE_SIZE;
use ferro_db::storage::buffer_pool::BufferPoolManager;
use ferro_db::storage::rid::Rid;
use ferro_db::storage::slotted_page::SlottedPage;

fn main() -> Result<(), Box<dyn Error>> {
    let mut bpm = BufferPoolManager::new(2, PAGE_SIZE, "data.db")?;
    let page_id = 0;
    let mut rids = Vec::new();

    {
        let page = bpm.fetch_page(page_id)?;
        let mut slotted = SlottedPage::init(page.data_mut())?;
        for name in ["alice", "bob", "carol"] {
            let slot_id = slotted.insert(name.as_bytes())?;
            rids.push(Rid::new(page_id, slot_id));
        }
        slotted.delete(rids[1].slot_id)?;
    }
    bpm.unpin_page(page_id, true);

    // Two other pages fill the pool and force page 0 out to disk.
    for other in 1..=2 {
        bpm.fetch_page(other)?;
        bpm.unpin_page(other, false);
    }

    let page = bpm.fetch_page(page_id)?;
    let slotted = SlottedPage::new(page.data())?;
    for rid in &rids {
        match slotted.get(rid.slot_id) {
            Ok(tuple) => println!("{rid} -> {}", String::from_utf8_lossy(tuple)),
            Err(err) => println!("{rid} -> {err}"),
        }
    }
    bpm.unpin_page(page_id, false);

    Ok(())
}
