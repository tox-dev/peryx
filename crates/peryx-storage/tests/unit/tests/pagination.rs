#[derive(Debug, Eq, PartialEq)]
pub struct Page {
    pub keys: Vec<String>,
    pub next_cursor: Option<String>,
}

pub fn pages_after_exclusive_cursor<E>(
    keys: &[&str],
    mut seed: impl FnMut(&str),
    mut list: impl FnMut(Option<String>, usize) -> Result<Page, E>,
    limit: usize,
) -> Result<(Page, Page), E> {
    for key in keys {
        seed(key);
    }
    let first = list(None, limit)?;
    let second = list(first.next_cursor.clone(), limit)?;
    Ok((first, second))
}

pub fn terminal_page<E>(
    keys: &[&str],
    mut seed: impl FnMut(&str),
    list: impl FnOnce(Option<String>, usize) -> Result<Page, E>,
) -> Result<Page, E> {
    for key in keys {
        seed(key);
    }
    list(None, keys.len())
}
