use std::path::Path;

pub fn p(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}
