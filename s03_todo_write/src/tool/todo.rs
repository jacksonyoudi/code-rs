use serde::Deserialize;
use strum::EnumProperty as _;
use strum_macros::EnumProperty;

#[derive(EnumProperty, PartialEq, Eq, Clone, Debug, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PLanItemStatus {
    #[strum(props(marker = "[ ]"))]
    Pending,
    #[strum(props(marker = "[>]"))]
    InProgress,
    #[strum(props(marker = "[x]"))]
    Completed,
}
