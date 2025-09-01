use std::{collections::HashMap, fmt, path::PathBuf, sync::Arc};

use better_command::NixHandler;
use serde::{Serialize, de::DeserializeOwned};

use crate::{Result, Value, nix_go};
