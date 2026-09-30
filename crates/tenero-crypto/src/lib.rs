//! Tenero's wrappers around audited proof libraries (M7). Experimental.

#[cfg(test)]
mod tests {
    #[test]
    fn libraries_link() {
        let _ = std::any::type_name::<monero_bulletproofs::Bulletproof>();
        let _ = std::any::type_name::<monero_clsag::Clsag>();
    }
}
