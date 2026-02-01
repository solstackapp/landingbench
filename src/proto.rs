#![allow(clippy::all, dead_code)]

#[allow(clippy::all, non_camel_case_types, non_snake_case)]
pub mod geyser {
    include!(concat!(env!("OUT_DIR"), "/geyser.rs"));
}

#[allow(clippy::all, non_camel_case_types, non_snake_case)]
pub mod solana {
    #[allow(clippy::all, non_camel_case_types, non_snake_case)]
    pub mod storage {
        #[allow(clippy::all, non_camel_case_types, non_snake_case)]
        pub mod confirmed_block {
            include!(concat!(
                env!("OUT_DIR"),
                "/solana.storage.confirmed_block.rs"
            ));
        }
    }
}
