//! bead: quern-catalog — table name -> Schema
//!
//! The catalog is the in-memory map from a table name to its [`Schema`], plus
//! a byte encoding so it can be persisted. It deliberately knows nothing about
//! `pager`/`storage`: [`Catalog::to_bytes`] hands out a buffer and
//! [`Catalog::from_bytes`] takes one back, and where those bytes live (the
//! pager's header page) is entirely the storage layer's business.
//!
//! Table names are case-insensitive (§1). The map key is the ASCII-lowercased
//! name; `Schema::table` keeps the spelling the user typed, because that is
//! what error messages and `.slt` output should show.

use std::collections::HashMap;

use crate::types::{Column, QuernError, Result, Schema, Type};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalog {
    /// key: `table.to_ascii_lowercase()`; value keeps the original spelling.
    tables: HashMap<String, Schema>,
}

impl Catalog {
    pub fn new() -> Self {
        Self::default()
    }

    /// `Err(Catalog)` if a table of that name (in any casing) already exists.
    pub fn create(&mut self, schema: Schema) -> Result<()> {
        let key = schema.table.to_ascii_lowercase();
        if let Some(existing) = self.tables.get(&key) {
            return Err(QuernError::Catalog(format!(
                "table {} already exists",
                existing.table
            )));
        }
        self.tables.insert(key, schema);
        Ok(())
    }

    /// `Err(Catalog)` if the table is unknown.
    pub fn get(&self, table: &str) -> Result<&Schema> {
        self.tables
            .get(&table.to_ascii_lowercase())
            .ok_or_else(|| Self::no_such_table(table))
    }

    /// Removes the table and returns its schema, so a caller that has to tear
    /// down storage for it does not need a second `get` first.
    /// `Err(Catalog)` if the table is unknown.
    // Not `Drop::drop`: two arguments and a Result, so clippy's
    // should_implement_trait does not apply. The name is the SQL verb.
    pub fn drop(&mut self, table: &str) -> Result<Schema> {
        self.tables
            .remove(&table.to_ascii_lowercase())
            .ok_or_else(|| Self::no_such_table(table))
    }

    /// Every schema, ordered by normalised table name. Sorted rather than in
    /// `HashMap` order so both the byte encoding and anything user-visible are
    /// deterministic (§5).
    pub fn list(&self) -> Vec<&Schema> {
        let mut keyed: Vec<(&String, &Schema)> = self.tables.iter().collect();
        keyed.sort_by(|a, b| a.0.cmp(b.0));
        keyed.into_iter().map(|(_, s)| s).collect()
    }

    fn no_such_table(table: &str) -> QuernError {
        QuernError::Catalog(format!("no such table: {table}"))
    }

    // --- encoding ----------------------------------------------------------
    //
    // Boring and explicit, because these bytes go to disk and come back from
    // an untrusted file:
    //
    //   catalog := u32 table_count, table*
    //   table   := string name, u32 column_count, column*
    //   column  := string name, u8 type_tag, u8 primary_key
    //   string  := u32 byte_len, utf8 bytes
    //
    // All integers little-endian. Tables are written in sorted order, so the
    // same catalog always encodes to the same bytes.

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let tables = self.list();
        out.extend_from_slice(&(tables.len() as u32).to_le_bytes());
        for schema in tables {
            put_str(&mut out, &schema.table);
            out.extend_from_slice(&(schema.columns.len() as u32).to_le_bytes());
            for col in &schema.columns {
                put_str(&mut out, &col.name);
                out.push(type_tag(col.ty));
                out.push(u8::from(col.primary_key));
            }
        }
        out
    }

    /// Parses bytes written by [`Catalog::to_bytes`]. This is a trust boundary
    /// — the buffer comes off disk — so every length is checked before it is
    /// used and a truncated or corrupt buffer is `Err(Catalog)`, never a panic.
    ///
    /// Bytes after the last table are ignored: the header page is a fixed 4096
    /// bytes, so the tail is padding. That also makes an all-zero page decode
    /// as an empty catalog, which is what a freshly created file holds.
    pub fn from_bytes(bytes: &[u8]) -> Result<Catalog> {
        let mut r = Reader { b: bytes, pos: 0 };
        let n = r.u32()?;
        let mut catalog = Catalog::new();
        for _ in 0..n {
            let table = r.string()?;
            let ncols = r.u32()?;
            let mut columns = Vec::new();
            for _ in 0..ncols {
                let name = r.string()?;
                let ty = match r.u8()? {
                    0 => Type::Int,
                    1 => Type::Text,
                    2 => Type::Bool,
                    tag => {
                        return Err(QuernError::Catalog(format!(
                            "corrupt catalog: unknown type tag {tag}"
                        )))
                    }
                };
                let primary_key = match r.u8()? {
                    0 => false,
                    1 => true,
                    flag => {
                        return Err(QuernError::Catalog(format!(
                            "corrupt catalog: bad primary-key flag {flag}"
                        )))
                    }
                };
                columns.push(Column {
                    name,
                    ty,
                    primary_key,
                });
            }
            // Routing through `create` rejects a buffer that names the same
            // table twice instead of silently keeping the last one.
            catalog.create(Schema { table, columns })?;
        }
        Ok(catalog)
    }
}

fn type_tag(ty: Type) -> u8 {
    match ty {
        Type::Int => 0,
        Type::Text => 1,
        Type::Bool => 2,
    }
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn truncated() -> QuernError {
        QuernError::Catalog("corrupt catalog: buffer truncated".into())
    }

    /// The only place that slices, so it is the only place that can be short.
    /// `get` returns `None` rather than panicking on any out-of-range length,
    /// including an absurd one, and nothing is allocated before the check.
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or_else(Self::truncated)?;
        let slice = self.b.get(self.pos..end).ok_or_else(Self::truncated)?;
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn string(&mut self) -> Result<String> {
        let len = self.u32()? as usize;
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec())
            .map_err(|e| QuernError::Catalog(format!("corrupt catalog: invalid utf-8: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema(table: &str, cols: &[(&str, Type, bool)]) -> Schema {
        Schema {
            table: table.to_string(),
            columns: cols
                .iter()
                .map(|(name, ty, pk)| Column {
                    name: name.to_string(),
                    ty: *ty,
                    primary_key: *pk,
                })
                .collect(),
        }
    }

    fn sample() -> Catalog {
        let mut c = Catalog::new();
        c.create(schema(
            "Users",
            &[("id", Type::Int, true), ("name", Type::Text, false)],
        ))
        .unwrap();
        c.create(schema("empty", &[])).unwrap();
        c.create(schema(
            "t",
            &[
                ("a", Type::Int, false),
                ("b", Type::Text, false),
                ("c", Type::Bool, false),
            ],
        ))
        .unwrap();
        c
    }

    #[test]
    fn create_get_drop_and_list() {
        let mut c = sample();
        assert_eq!(c.get("t").unwrap().columns.len(), 3);
        assert_eq!(
            c.list()
                .iter()
                .map(|s| s.table.as_str())
                .collect::<Vec<_>>(),
            vec!["empty", "t", "Users"] // sorted by lowercased name
        );
        let dropped = c.drop("t").unwrap();
        assert_eq!(dropped.table, "t");
        assert!(c.get("t").is_err());
        assert_eq!(c.list().len(), 2);
    }

    #[test]
    fn duplicate_create_and_missing_table_are_catalog_errors() {
        let mut c = sample();
        assert!(matches!(
            c.create(schema("t", &[])),
            Err(QuernError::Catalog(_))
        ));
        assert!(matches!(c.get("nope"), Err(QuernError::Catalog(_))));
        assert!(matches!(c.drop("nope"), Err(QuernError::Catalog(_))));
        // the duplicate did not clobber the original
        assert_eq!(c.get("t").unwrap().columns.len(), 3);
    }

    #[test]
    fn names_are_case_insensitive_but_spelling_is_preserved() {
        let mut c = sample();
        assert_eq!(c.get("USERS").unwrap().table, "Users");
        assert_eq!(c.get("users").unwrap().table, "Users");
        assert!(matches!(
            c.create(schema("USERS", &[])),
            Err(QuernError::Catalog(_))
        ));
        assert!(c.drop("uSeRs").is_ok());
        assert!(c.get("Users").is_err());
    }

    #[test]
    fn round_trips_through_bytes() {
        let c = sample();
        let bytes = c.to_bytes();
        assert_eq!(Catalog::from_bytes(&bytes).unwrap(), c);
        // and the encoding is stable
        assert_eq!(Catalog::from_bytes(&bytes).unwrap().to_bytes(), bytes);
        // an empty catalog, and a zeroed header page, are both empty
        assert_eq!(
            Catalog::from_bytes(&Catalog::new().to_bytes()).unwrap(),
            Catalog::new()
        );
        assert_eq!(Catalog::from_bytes(&[0u8; 4096]).unwrap(), Catalog::new());
    }

    #[test]
    fn malformed_buffers_are_errors_not_panics() {
        let bytes = sample().to_bytes();
        // every truncation of a valid buffer
        for n in 0..bytes.len() {
            assert!(
                Catalog::from_bytes(&bytes[..n]).is_err(),
                "truncation at {n} must be an error"
            );
        }
        // a length prefix far bigger than the buffer
        let huge = [1, 0, 0, 0, 0xff, 0xff, 0xff, 0xff];
        assert!(matches!(
            Catalog::from_bytes(&huge),
            Err(QuernError::Catalog(_))
        ));
        // one table, one column, bogus type tag
        let bad_tag = [
            1, 0, 0, 0, 1, 0, 0, 0, b't', 1, 0, 0, 0, 1, 0, 0, 0, b'a', 9, 0,
        ];
        assert!(matches!(
            Catalog::from_bytes(&bad_tag),
            Err(QuernError::Catalog(_))
        ));
        // ...same shape, bogus primary-key flag
        let bad_pk = [
            1, 0, 0, 0, 1, 0, 0, 0, b't', 1, 0, 0, 0, 1, 0, 0, 0, b'a', 0, 7,
        ];
        assert!(matches!(
            Catalog::from_bytes(&bad_pk),
            Err(QuernError::Catalog(_))
        ));
        // invalid utf-8 in a name
        let bad_utf8 = [1, 0, 0, 0, 1, 0, 0, 0, 0xff, 0, 0, 0, 0];
        assert!(matches!(
            Catalog::from_bytes(&bad_utf8),
            Err(QuernError::Catalog(_))
        ));
        // the same table twice
        let dup = [
            2, 0, 0, 0, 1, 0, 0, 0, b't', 0, 0, 0, 0, 1, 0, 0, 0, b'T', 0, 0, 0, 0,
        ];
        assert!(matches!(
            Catalog::from_bytes(&dup),
            Err(QuernError::Catalog(_))
        ));
        // garbage
        assert!(Catalog::from_bytes(b"not a catalog at all").is_err());
    }
}
