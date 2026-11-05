// SPDX-License-Identifier: AGPL-3.0-only
//! Generate or verify the protocol's JSON Schema documents.

use anyhow::{Context as _, bail};
use std::{collections::BTreeSet, fs, path::Path};

pub(crate) fn run(output: &Path, check: bool) -> anyhow::Result<()> {
    let documents = fleet_protocol::schema::documents();
    if !check {
        fs::create_dir_all(output)?;
    }
    let mut expected = BTreeSet::new();
    for (name, schema) in documents {
        let name = format!("{name}.json");
        expected.insert(name.clone());
        let path = output.join(name);
        let mut contents = serde_json::to_string_pretty(&schema)?;
        contents.push('\n');
        if check {
            let actual =
                fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
            if actual != contents {
                bail!("stale schema {}; run cargo xtask schema", path.display());
            }
        } else {
            fs::write(&path, contents).with_context(|| format!("writing {}", path.display()))?;
        }
    }
    if check {
        for entry in fs::read_dir(output)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry
                .path()
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
                && !expected.contains(&name)
            {
                bail!("unexpected schema {}", entry.path().display());
            }
        }
    }
    Ok(())
}
