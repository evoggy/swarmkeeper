//! TOC cache on disk, shared by every connection.


#[derive(Clone)]
pub(crate) struct FileTocCache {
    pub(crate) cache_dir: std::path::PathBuf,
}

impl FileTocCache {
    pub(crate) fn new() -> Self {
        let cache_dir = dirs_next::cache_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join("swarmkeeper")
            .join("toc_cache");
        std::fs::create_dir_all(&cache_dir).ok();
        FileTocCache { cache_dir }
    }

    pub(crate) fn path_for(&self, key: &[u8]) -> std::path::PathBuf {
        let hex: String = key.iter().map(|b| format!("{:02x}", b)).collect();
        self.cache_dir.join(format!("{}.json", hex))
    }
}

impl crazyflie_lib::TocCache for FileTocCache {
    fn get_toc(&self, key: &[u8]) -> Option<String> {
        std::fs::read_to_string(self.path_for(key)).ok()
    }

    fn store_toc(&self, key: &[u8], toc: &str) {
        std::fs::write(self.path_for(key), toc).ok();
    }
}
