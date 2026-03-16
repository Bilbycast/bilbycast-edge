//! SMPTE 2022-1 FEC Matrix -- L x D grid for XOR parity computation.
//!
//! Layout: L columns x D rows. Media packets are placed column-major:
//!   position = (seq - base_seq) % (L * D)
//!   column  = position % L
//!   row     = position / L
//!
//! Column FEC covers D packets in the same column (stride L).
//! Row FEC covers L packets in the same row (stride 1).

/// L x D matrix for FEC computation.
///
/// Stores media packet payloads in a flat `Vec<Option<Vec<u8>>>` indexed by
/// position (0..L*D). Running XOR parity accumulators are maintained per
/// column and per row so that a single missing packet in any complete
/// column or row can be recovered instantly.
pub struct FecMatrix {
    /// Number of columns (L parameter)
    pub columns: u8,
    /// Number of rows (D parameter)
    pub rows: u8,
    /// Media packet payloads indexed by position (size = L * D)
    cells: Vec<Option<Vec<u8>>>,
    /// Running XOR accumulator for each column (size = L)
    col_parity: Vec<Vec<u8>>,
    /// Running XOR accumulator for each row (size = D)
    row_parity: Vec<Vec<u8>>,
    /// Count of received packets per column
    col_count: Vec<u8>,
    /// Count of received packets per row
    row_count: Vec<u8>,
    /// Max payload length seen (for padding shorter payloads with zeros during XOR)
    max_payload_len: usize,
}

impl FecMatrix {
    /// Create a new FEC matrix with the given dimensions.
    ///
    /// Allocates `columns * rows` cell slots and separate parity
    /// accumulators for each column and each row.
    pub fn new(columns: u8, rows: u8) -> Self {
        let size = columns as usize * rows as usize;
        Self {
            columns,
            rows,
            cells: vec![None; size],
            col_parity: vec![Vec::new(); columns as usize],
            row_parity: vec![Vec::new(); rows as usize],
            col_count: vec![0; columns as usize],
            row_count: vec![0; rows as usize],
            max_payload_len: 0,
        }
    }

    /// Matrix size (L * D)
    pub fn size(&self) -> usize {
        self.columns as usize * self.rows as usize
    }

    /// Insert a media packet at the given matrix position.
    /// Returns the (column, row) indices.
    pub fn insert_media(&mut self, position: usize, payload: &[u8]) -> (u8, u8) {
        let col = (position % self.columns as usize) as u8;
        let row = (position / self.columns as usize) as u8;

        if position < self.size() && self.cells[position].is_none() {
            self.cells[position] = Some(payload.to_vec());

            // Update max payload length
            if payload.len() > self.max_payload_len {
                self.max_payload_len = payload.len();
            }

            // XOR into column parity
            Self::xor_accumulate(&mut self.col_parity[col as usize], payload);
            self.col_count[col as usize] += 1;

            // XOR into row parity
            Self::xor_accumulate(&mut self.row_parity[row as usize], payload);
            self.row_count[row as usize] += 1;
        }

        (col, row)
    }

    /// Insert a column FEC packet (received from the FEC stream).
    pub fn insert_col_fec(&mut self, col_idx: u8, payload: &[u8]) {
        // The column FEC is the XOR of all D packets in that column.
        // We XOR it into the running parity — if we already have D-1 packets,
        // the result will be the missing packet.
        Self::xor_accumulate(&mut self.col_parity[col_idx as usize], payload);
        if payload.len() > self.max_payload_len {
            self.max_payload_len = payload.len();
        }
    }

    /// Insert a row FEC packet (received from the FEC stream).
    pub fn insert_row_fec(&mut self, row_idx: u8, payload: &[u8]) {
        Self::xor_accumulate(&mut self.row_parity[row_idx as usize], payload);
        if payload.len() > self.max_payload_len {
            self.max_payload_len = payload.len();
        }
    }

    /// Try to recover a missing packet in the given column.
    /// Returns the recovered payload if exactly 1 packet is missing AND we have the column FEC.
    pub fn try_recover_column(&self, col_idx: u8) -> Option<Vec<u8>> {
        let col = col_idx as usize;
        let expected = self.rows;

        // We need exactly (D - 1) media packets + the FEC packet
        // After XOR-ing all received media packets and the FEC packet into col_parity,
        // if only 1 is missing, the parity IS the missing packet.
        if self.col_count[col] == expected - 1 && !self.col_parity[col].is_empty() {
            Some(self.col_parity[col].clone())
        } else {
            None
        }
    }

    /// Try to recover a missing packet in the given row.
    /// Returns the recovered payload if exactly 1 packet is missing AND we have the row FEC.
    pub fn try_recover_row(&self, row_idx: u8) -> Option<Vec<u8>> {
        let row = row_idx as usize;
        let expected = self.columns;

        if self.row_count[row] == expected - 1 && !self.row_parity[row].is_empty() {
            Some(self.row_parity[row].clone())
        } else {
            None
        }
    }

    /// Check if a cell at the given position has data.
    pub fn has_cell(&self, position: usize) -> bool {
        position < self.size() && self.cells[position].is_some()
    }

    /// Get the position's column and row indices.
    pub fn position_to_col_row(&self, position: usize) -> (u8, u8) {
        let col = (position % self.columns as usize) as u8;
        let row = (position / self.columns as usize) as u8;
        (col, row)
    }

    /// Clear the matrix for the next cycle.
    pub fn reset(&mut self) {
        for cell in &mut self.cells {
            *cell = None;
        }
        for p in &mut self.col_parity {
            p.clear();
        }
        for p in &mut self.row_parity {
            p.clear();
        }
        for c in &mut self.col_count {
            *c = 0;
        }
        for c in &mut self.row_count {
            *c = 0;
        }
        self.max_payload_len = 0;
    }

    /// Generate column FEC payload for the given column.
    /// Returns the XOR of all D packets in that column, or None if the column is incomplete.
    pub fn generate_col_fec(&self, col_idx: u8) -> Option<Vec<u8>> {
        let col = col_idx as usize;
        if self.col_count[col] == self.rows {
            Some(self.col_parity[col].clone())
        } else {
            None
        }
    }

    /// Generate row FEC payload for the given row.
    /// Returns the XOR of all L packets in that row, or None if the row is incomplete.
    pub fn generate_row_fec(&self, row_idx: u8) -> Option<Vec<u8>> {
        let row = row_idx as usize;
        if self.row_count[row] == self.columns {
            Some(self.row_parity[row].clone())
        } else {
            None
        }
    }

    /// XOR `source` into `target`, extending target if needed.
    fn xor_accumulate(target: &mut Vec<u8>, source: &[u8]) {
        if target.len() < source.len() {
            target.resize(source.len(), 0);
        }
        for (t, s) in target.iter_mut().zip(source.iter()) {
            *t ^= *s;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_xor_accumulate_basic() {
        let mut target = vec![0u8; 4];
        FecMatrix::xor_accumulate(&mut target, &[0xFF, 0x00, 0xAA, 0x55]);
        assert_eq!(target, vec![0xFF, 0x00, 0xAA, 0x55]);

        // XOR again with same data should produce zeros
        FecMatrix::xor_accumulate(&mut target, &[0xFF, 0x00, 0xAA, 0x55]);
        assert_eq!(target, vec![0x00, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn test_xor_recovery_single_missing() {
        // 3 columns (L=3), 2 rows (D=2) => 6 cells
        let mut matrix = FecMatrix::new(3, 2);

        // Payloads: A=0x01, B=0x02, C=0x03, D=0x04, E=0x05, F=0x06
        // Column 0: positions 0, 3 (rows 0, 1)
        // Column 1: positions 1, 4
        // Column 2: positions 2, 5

        let a = vec![0x01, 0x10];
        let b = vec![0x02, 0x20];
        let c = vec![0x03, 0x30];
        let d = vec![0x04, 0x40];
        // E is "missing" (position 4, column 1, row 1)
        let f = vec![0x06, 0x60];

        matrix.insert_media(0, &a);
        matrix.insert_media(1, &b);
        matrix.insert_media(2, &c);
        matrix.insert_media(3, &d);
        // Skip position 4 (E is lost)
        matrix.insert_media(5, &f);

        // Column 1 has: B (pos 1) but not E (pos 4)
        // Column FEC for column 1 = B XOR E = [0x02^0x05, 0x20^0x50] = [0x07, 0x70]
        let col1_fec = vec![0x07, 0x70]; // pre-computed B XOR E
        matrix.insert_col_fec(1, &col1_fec);

        // Try to recover column 1
        let recovered = matrix.try_recover_column(1);
        assert!(recovered.is_some());
        let recovered = recovered.unwrap();
        // The parity now contains: B XOR (B XOR E) = E
        // B = [0x02, 0x20], FEC = [0x07, 0x70]
        // parity = B XOR FEC = [0x02^0x07, 0x20^0x70] = [0x05, 0x50]
        assert_eq!(recovered, vec![0x05, 0x50]); // This is E!
    }

    #[test]
    fn test_row_recovery() {
        // L=2, D=2 => 4 cells
        let mut matrix = FecMatrix::new(2, 2);

        // Row 0: positions 0, 1
        // Row 1: positions 2, 3
        let a = vec![0xAA];
        let b = vec![0xBB];
        // C at position 2 is missing
        let d = vec![0xDD];

        matrix.insert_media(0, &a);
        matrix.insert_media(1, &b);
        // Skip position 2
        matrix.insert_media(3, &d);

        // Row 1 FEC = C XOR D
        let c = vec![0xCC];
        let row1_fec: Vec<u8> = c.iter().zip(d.iter()).map(|(x, y)| x ^ y).collect();
        matrix.insert_row_fec(1, &row1_fec);

        let recovered = matrix.try_recover_row(1);
        assert!(recovered.is_some());
        // parity contains: D XOR row_fec = D XOR (C XOR D) = C
        assert_eq!(recovered.unwrap(), vec![0xCC]);
    }

    #[test]
    fn test_generate_col_fec() {
        let mut matrix = FecMatrix::new(2, 3); // L=2, D=3

        // Column 0: positions 0, 2, 4
        matrix.insert_media(0, &[0x01]);
        matrix.insert_media(2, &[0x02]);
        matrix.insert_media(4, &[0x04]);

        let fec = matrix.generate_col_fec(0);
        assert!(fec.is_some());
        // 0x01 ^ 0x02 ^ 0x04 = 0x07
        assert_eq!(fec.unwrap(), vec![0x07]);
    }

    #[test]
    fn test_generate_row_fec() {
        let mut matrix = FecMatrix::new(3, 2); // L=3, D=2

        // Row 0: positions 0, 1, 2
        matrix.insert_media(0, &[0x10]);
        matrix.insert_media(1, &[0x20]);
        matrix.insert_media(2, &[0x30]);

        let fec = matrix.generate_row_fec(0);
        assert!(fec.is_some());
        // 0x10 ^ 0x20 ^ 0x30 = 0x00
        assert_eq!(fec.unwrap(), vec![0x00]);
    }

    #[test]
    fn test_reset_clears_matrix() {
        let mut matrix = FecMatrix::new(2, 2);
        matrix.insert_media(0, &[0xFF]);
        assert!(matrix.has_cell(0));

        matrix.reset();
        assert!(!matrix.has_cell(0));
        assert_eq!(matrix.col_count[0], 0);
        assert_eq!(matrix.row_count[0], 0);
    }

    #[test]
    fn test_no_recovery_when_too_many_missing() {
        let mut matrix = FecMatrix::new(2, 3); // L=2, D=3

        // Column 0: only insert 1 of 3 (need 2 missing = can't recover)
        matrix.insert_media(0, &[0x01]);
        // positions 2 and 4 are missing

        // Even with FEC, can't recover 2 missing
        let col_fec = vec![0x01 ^ 0x02 ^ 0x04]; // all 3 XOR'd
        matrix.insert_col_fec(0, &col_fec);

        let recovered = matrix.try_recover_column(0);
        assert!(recovered.is_none()); // col_count=1, need D-1=2
    }
}
