mod buffer_pool;
mod disk_manager;
mod page;
mod replacer;

use buffer_pool::BufferPoolManager;

fn main() {
    let mut bpm = BufferPoolManager::new(2, 4096);

    {
        let page = bpm.fetch_page(1).unwrap();
        page.data_mut()[0] = 42;
    }
    bpm.unpin_page(1, true);

    {
        let page = bpm.fetch_page(2).unwrap();
        page.data_mut()[0] = 43;
    }
    bpm.unpin_page(2, true);

    {
        bpm.fetch_page(3);
    }
    bpm.unpin_page(3, false);

    {
        let page = bpm.fetch_page(2).unwrap();
        println!("{}", page.data()[0]);
    }
    bpm.unpin_page(2, false);

    let page = bpm.fetch_page(1).unwrap();
    println!("{}", page.data()[0]);
}
