//! Nur tatsächlich vorhandene Lochgrenzen brauchen ganze lokale Allokationsblöcke.
use crate::metadata::DataExtent;
use std::io;

pub(super) fn has_holes(extents: &[DataExtent], size: u64) -> bool {
    let mut end = 0;
    for extent in extents {
        if extent.offset > end {
            return true;
        }
        end = extent.offset.saturating_add(extent.length);
    }
    end < size
}
pub(super) fn aligned(extents: &[DataExtent], size: u64, unit: u32) -> io::Result<bool> {
    if !unit.is_power_of_two() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Ungültige Sparse-Allokationsblockgröße",
        ));
    }
    let unit = u64::from(unit);
    let mut end = 0;
    let mut aligned = true;
    for extent in extents {
        let next = extent
            .offset
            .checked_add(extent.length)
            .filter(|next| *next <= size)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Sparse-Bereich liegt außerhalb der bestätigten Größe",
                )
            })?;
        if extent.length == 0 || extent.offset < end {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Ungültige Sparse-Bereichsfolge",
            ));
        }
        if extent.offset > end {
            aligned &= end.is_multiple_of(unit) && extent.offset.is_multiple_of(unit);
        }
        end = next;
    }
    // Ein letzter Datenblock endet logisch am EOF; seine physische Rundung liegt
    // außerhalb der Datei. Ein echtes nachfolgendes Loch benötigt dagegen Alignment.
    if end < size {
        aligned &= end.is_multiple_of(unit);
    }
    Ok(aligned)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ranges(values: &[(u64, u64)]) -> Vec<DataExtent> {
        values
            .iter()
            .map(|&(offset, length)| DataExtent { offset, length })
            .collect()
    }
    #[test]
    fn data_runs_and_eof_are_distinct_from_real_hole_boundaries() {
        for (values, size, expected) in [
            (vec![], 0, true),
            (vec![], 513, true),
            (vec![(0, 513)], 513, true),
            (vec![(512, 512)], 8192, false),
            (vec![(0, 512)], 8192, false),
            (vec![(4096, 4096)], 8193, true),
            (vec![(0, 512), (512, 3584)], 8192, true),
            (vec![(0, 4096), (8192, 1)], 8193, true),
            (vec![(0, 4096), (8191, 2)], 8193, false),
        ] {
            assert_eq!(
                aligned(&ranges(&values), size, 4096).unwrap(),
                expected,
                "{values:?} size={size}"
            );
        }
        assert!(!has_holes(&ranges(&[(0, 512), (512, 3584)]), 4096));
        assert!(has_holes(&[], 513));
    }
    #[test]
    fn malformed_sizes_never_become_metadata_omissions() {
        assert!(aligned(&[], 1, 0).is_err());
        assert!(aligned(&[], 1, 3).is_err());
        for values in [
            vec![(0, 0)],
            vec![(8192, 1)],
            vec![(0, 100), (99, 1)],
            vec![(u64::MAX, 2)],
        ] {
            assert!(aligned(&ranges(&values), 8192, 4096).is_err());
        }
    }
}
