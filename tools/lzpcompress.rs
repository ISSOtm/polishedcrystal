/* # Introduction
 *
 * LZ (Lempel-Ziv) compression works by generating a stream of *commands* that attempt to more
 * efficiently encode the raw data.
 * For example, “emit <byte>, emit <byte>, emit <byte>” can be expressed more compactly as
 * “emit <byte> 3 times”.
 *
 * There are a lot of variants of LZ compression due to the great flexibility of such a scheme;
 * Polished Crystal's variant evolved from Pokémon Crystal's original LZ (itself very similar to
 * LC_LZ3 [^LC_LZ3]), with additional commands and encoding tweaks to fit Game Boy graphics better.
 *
 * [^LC_LZ3]: “LC_LZ3 Compression Format”
 * https://github.com/bonimy/MushROMs/blob/master/doc/LC_LZ3%20Compression%20Format.md
 */

/* # Command stream
 *
 * Commands are encoded in a less obvious, but far more space-efficient manner.
 * Each command begins with a variable-length header indicating the command and its length,
 * followed by any payload bytes; the next command follows immediately.
 * The stream ends with a special header byte, at which the decompressor stops.
 *
 * The header byte(s) are interpreted in very different ways depending on some of their bits;
 * the various encodings are described first, followed by the decoder's decision tree.
 *
 * ## Command encodings
 *
 * There are seven “simple” LZ commands, which are thus encoded in three bits,
 * with the eigth value triggering some other special behavior.
 *
 * These commands have a “short”, 1-byte encoding:
 *   %cccl_llll
 * The three `c` bits indicate the command, and the five `l` bits encode a length from 0 to 31.
 *
 * They also have a “long”, 2-byte encoding:
 *   %111c_cc0l llll_llll
 * The length is expanded to 9 bits (0–511); the 10th bit is unused because that allows massively
 * speeding up the decompressor's loops. [^9bit_len]
 *
 * There are also some “special” commands:
 *   %1111_11cc llll_llll
 * ...although %1111_1111 instead indicates the end of the stream, and does not have a length byte.
 *
 * [^9bit_len]: “Shorten long lz counts to 9 bits”
 * https://github.com/Rangi42/polishedcrystal/commit/2d48f4f2314fd9e0e91f6df3d36ba87edd8e581d
 *
 * ## Decoding decision tree
 *
 * First, a header byte is read, and checked for “short” encoding: if the `ccc` field is not the
 * reserved value 7, then we have a short command and can process it.
 *
 * Otherwise, check if the byte begins a “long” encoding: if that `ccc` isn't the reserved value 7,
 * then we have a long command and can read the extra byte before processing it.
 *
 * Otherwise (the top 6 bits of the first byte are all set), we must have a special command.
 * First, if it is the terminator byte, simply exit.
 * Otherwise, read the special command's length, and process it.
 *
 * ## Command payloads
 *
 * Each command may have extra payload bytes after its header, but this is instead documented right
 * above each command's `serialize()` function.
 */

use std::{
    assert_matches,
    borrow::Cow,
    collections::VecDeque,
    fs::File,
    io::{Read, Write},
    num::IntErrorKind,
    ops::RangeInclusive,
    path::{Path, PathBuf},
};

// From <sysexits.h>, which is just an arbitrarily-picked convention.
const EXIT_SUCCESS: i32 = 0;
const EXIT_BAD_USAGE: i32 = 64;
const EXIT_DATA_ERR: i32 = 65;
const EXIT_MISSING_INPUT: i32 = 66;
const EXIT_OS_ERR: i32 = 71;
const EXIT_CANT_CREATE: i32 = 73;
const EXIT_IO_ERR: i32 = 74;
fn main() {
    let options = Options::parse_cli();
    let (res, input_filename) = match Options::path_arg(options.input.as_deref()) {
        Some(path) => (std::fs::read(path), path),
        None => {
            let mut buf = Vec::with_capacity(0x1000);
            (
                std::io::stdin().lock().read_to_end(&mut buf).map(|_| buf),
                "<standard input>".as_ref(),
            )
        }
    };
    let input_bytes = match res {
        Err(err) => {
            eprintln!(
                "error: while reading \"{}\": {err}",
                input_filename.display(),
            );
            std::process::exit(EXIT_IO_ERR);
        }
        Ok(bytes) => bytes,
    };

    let (output, output_filename) = match Options::path_arg(options.output.as_deref()) {
        Some(path) => match File::create(path) {
            Err(err) => {
                eprintln!(
                    "error: unable to create output file \"{}\": {err}",
                    path.display()
                );
                std::process::exit(EXIT_CANT_CREATE);
            }
            Ok(file) => (OutputFile::File(file), path),
        },
        None => (
            OutputFile::Stdout(std::io::stdout().lock()),
            "<standard output>".as_ref(),
        ),
    };
    let res = match options.mode {
        OperationMode::Compress(output_mode) => {
            let commands = compress(&input_bytes);

            match output_mode {
                OutputMode::Binary => serialize(&commands, output),
                OutputMode::Textual => describe(&commands, &input_bytes, output),
            }
        }
        OperationMode::Decompress(output_mode) => {
            let commands = match deserialize(&input_bytes) {
                Ok(commands) => commands,
                Err(err) => {
                    eprintln!("error: cannot process input file: {err}");
                    std::process::exit(EXIT_DATA_ERR);
                }
            };
            let decompressed = decompress(&commands);

            todo!()
        }
    };
    if let Err(err) = res {
        eprintln!(
            "error: unable to write to \"{}\": {err}",
            output_filename.display(),
        );
        std::process::exit(EXIT_IO_ERR);
    }
}

fn usage(exit_code: i32, program_name: &str) {
    eprintln!(
        "Usage: {program_name} [<options>] [<source file> [<output>]]

Execution mode:
    -b, --binary      Output the command stream as binary data (default).
    -t, --text        Output the command stream as text.
    -u, --decompress  Process a compressed file and output the original data.
    -d, --dump        Process a compressed file and dump the command stream as
                      text (as if compressing with the --text option).
    -h, --help        Print this help text and exit.
Compression options:
    -a<number>, --align <number>   Pad the compressed output with zeros until
                                   the size has the specified number of low bits
                                   cleared (default: 0).
The source and output filenames can be given as - (or omitted) to use standard
input and output. Use -- to indicate that subsequent arguments are file names."
    );
    std::process::exit(exit_code);
}

#[derive(Debug)]
struct Options {
    input: Option<PathBuf>,
    output: Option<PathBuf>,
    mode: OperationMode,
    alignment_nb_bits: u8,
}
#[derive(Debug, Clone, Copy)]
enum OperationMode {
    Compress(OutputMode),
    Decompress(OutputMode),
}
#[derive(Debug, Clone, Copy)]
enum OutputMode {
    Binary,
    Textual,
}

/// Converts a length from Rust's expected ranges to what works on the GB.
// Since this compression format has been heavily tailored for the GB's graphics,
// it is reasonable to constrain it as such.
fn gb_len(rust_len: usize) -> u16 {
    try_gb_len(rust_len).unwrap()
}
fn try_gb_len(rust_len: usize) -> Option<u16> {
    // Note that we halve the range so that it is also friendly to signed operations;
    // Rust also refuses to allocate objects larger than `isize::MAX`, due to pointer diffing wraparound.
    (rust_len <= MAX_GB_LEN.into()).then_some(rust_len as u16)
}
const MAX_GB_LEN: u16 = i16::MAX as _;

// The meat of the compressor.

#[derive(Debug, Clone)]
enum Command<'decompressed> {
    Data(LzData<'decompressed>),
    Repeat(LzRepeat),
    Alternate(LzAlternate),
    Zero(LzZero),
    CopyNormal(LzCopyNormal),
    CopyFlipped(LzCopyFlipped),
    CopyReversed(LzCopyReversed),
    SpecialPackhi0(LzSpecialPackhi0<'decompressed>),
    SpecialPack16Short(LzSpecialPack16Short<'decompressed>),
    SpecialPacklo0(LzSpecialPacklo0<'decompressed>),
}
macro_rules! forward {
    (fn $method_name:ident (&self $(,)? $( $param_name:ident : $param_type:ty ),*) $(-> $ret_type:ty)?) => {
        fn $method_name(&self, $($param_name: $param_type),*) $(-> $ret_type)? {
            match self {
                Self::Data(inner) =>               inner.$method_name($($param_name),*),
                Self::Repeat(inner) =>             inner.$method_name($($param_name),*),
                Self::Alternate(inner) =>          inner.$method_name($($param_name),*),
                Self::Zero(inner) =>               inner.$method_name($($param_name),*),
                Self::CopyNormal(inner) =>         inner.$method_name($($param_name),*),
                Self::CopyFlipped(inner) =>        inner.$method_name($($param_name),*),
                Self::CopyReversed(inner) =>       inner.$method_name($($param_name),*),
                Self::SpecialPackhi0(inner) =>     inner.$method_name($($param_name),*),
                Self::SpecialPack16Short(inner) => inner.$method_name($($param_name),*),
                Self::SpecialPacklo0(inner) =>     inner.$method_name($($param_name),*),
            }
        }
    };
}
impl Command<'_> {
    forward! { fn assert_length_valid(&self) }
    forward! { fn decompressed_length(&self) -> u16 }
    forward! { fn num_encoded_bytes(&self) -> u16 }
    forward! { fn serialize(&self, output: impl Write) -> std::io::Result<()> }
    forward! { fn describe(&self, bytes: &[u8], output: impl Write) -> std::io::Result<()> }
    forward! { fn decompress(&self, dest: &mut Vec<u8>) }

    fn dummy() -> Self {
        Self::Data(LzData { bytes: &[] })
    }

    fn cost_of_chain(&self, prev_optimal_chains: &[(u16, Command<'_>)]) -> u16 {
        let cost_of_this = self.num_encoded_bytes();
        let len_decompressed_before =
            prev_optimal_chains.len() - usize::from(self.decompressed_length() - 1);
        let cost_of_chain_leading_to_this = len_decompressed_before
            .checked_sub(1)
            .map_or(0, |idx| prev_optimal_chains[idx].0);
        cost_of_this + cost_of_chain_leading_to_this
    }
}

fn compress<'data>(uncompressed: &'data [u8]) -> VecDeque<Command<'data>> {
    // TODO: reject inputs larger than `i16::MAX` as being infeasible to decompress on the GB(C).
    // (This compression format has been heavily tailored for the GB's characteristics.)

    // The strategy behind a “dynamic programming” LZ compressor stems from an interesting property:
    // the optimal command stream for the first N bytes is composed of one command, plus the optimal
    // stream for all bytes prior to this command.
    //
    // So, for each *end* position, consider each command that could end exactly at this position,
    // and consider the total cost to be
    // `optimal cost of ending at that command's start + cost of that command`.
    // Assuming that no command generates nothing (which, intuitively, we don't want), then that
    // optimal cost references an earlier end position, which will have been computed by a previous
    // iteration of the loop, induction-style!
    //
    // Once this is computed for every end position, take the optimal command that ends at the end
    // of the decompressed data, “rewind” by however many bytes it covers, and repeat until you
    // reach the beginning of the data!
    // (This feels similar to a Dijkstra path search, in a sense.)
    //
    // Of course, it's possible to implement the above strategy in less naive and more efficient
    // ways (mostly about more efficiently considering commands), but this remains the underlying
    // principle.

    // Each index corresponds to the best command that ends at this offset, and its associated cost.
    // The first entry is a dummy that encodes nothing but “seeds” all future iterations.
    let mut best_cmds = Vec::with_capacity(uncompressed.len()); // Can't use `.collect()` because each iteration depends on the previous.

    while best_cmds.len() != uncompressed.len() {
        // Place a dummy command that we know will be overwritten.
        const DUMMY_COST: u16 = u16::MAX;
        best_cmds.push((DUMMY_COST, Command::dummy()));
        let num_bytes_emitted = best_cmds.len();

        // Bytes that will have been emitted when the sought command is finished.
        let decompressed_thus_far = &uncompressed[..num_bytes_emitted];

        let ((lowest_cost, best_cmd), prev_optimal_chains) = best_cmds.split_last_mut().unwrap();
        // TODO: this short-circuiting is only performed *after* a valid command has been found...
        //       but constant-cost commands may be (expensively) attempted and discarded without reaching this!
        let mut consider = |candidate: Command<'data>| {
            // TODO: use this insteqd
            // let total_cost = candidate.cost_of_chain(prev_optimal_chains);
            let this_cost = candidate.num_encoded_bytes();
            let start_ofs = num_bytes_emitted - usize::from(candidate.decompressed_length());
            // Cost of this command, plus cost of the optimal command chain that ends where this one begins.
            let total_cost = this_cost
                + start_ofs
                    .checked_sub(1)
                    .map_or(0, |idx| prev_optimal_chains[idx].0);
            assert_matches!(total_cost, ..DUMMY_COST); // Ensures that the first considered command overwrites the dummy.

            // If this candidate would be a strict improvement, retain it.
            if total_cost < *lowest_cost {
                *lowest_cost = total_cost;
                *best_cmd = candidate;
                true
            } else {
                this_cost < *lowest_cost
            }
        };

        if decompressed_thus_far[decompressed_thus_far.len() - 1] == 0 {
            LzZero::for_each_possible_cmd(&mut consider, decompressed_thus_far);
        } else {
            LzRepeat::for_each_possible_cmd(&mut consider, decompressed_thus_far);
        }

        LzAlternate::for_each_possible_cmd(&mut consider, decompressed_thus_far);

        LzSpecialPackhi0::for_each_possible_cmd(&mut consider, decompressed_thus_far);
        LzSpecialPack16Short::for_each_possible_cmd(&mut consider, decompressed_thus_far);
        LzSpecialPacklo0::for_each_possible_cmd(&mut consider, decompressed_thus_far);

        for_each_possible_copy_cmd(&mut consider, decompressed_thus_far);

        // Consider `LzData` last, since its cost increases quickly, and thus the iteration may be
        // stopped very early if any of the previous commands succeeded at finding a more optimal
        // solution (and faster, too).
        LzData::for_each_possible_cmd(&mut consider, decompressed_thus_far);

        assert_ne!(*lowest_cost, DUMMY_COST); // The dummy command should at *least* have been overwritten by a `LzData`.
    }

    // Done! Now, we walk the list of commands (backwards).
    let mut best_path = VecDeque::new();
    let mut end_ofs = uncompressed.len();
    while end_ofs != 0 {
        let cmd = best_cmds[end_ofs - 1].1.clone();
        end_ofs -= usize::from(cmd.decompressed_length());
        best_path.push_front(cmd);
    }
    best_path
}

// Must decompress to a buffer, so that back-refs (`LzCopy`-derived commands) can access previous output.
fn decompress(commands: &[Command<'_>]) -> Vec<u8> {
    let mut decompressed = Vec::new();
    for command in commands {
        command.assert_length_valid();
        command.decompress(&mut decompressed);
    }
    // TODO: reject outputs larger than `i16::MAX` as being infeasible to decompress on the GB(C).
    // (This compression format has been heavily tailored for the GB's characteristics.)
    decompressed
}

fn serialize(commands: &VecDeque<Command<'_>>, mut output: impl Write) -> std::io::Result<()> {
    for command in commands {
        command.assert_length_valid();
        command.serialize(&mut output)?;
    }

    output.write_all(&[LzSpecialCmdId::End as u8])
}
fn describe(
    commands: &VecDeque<Command<'_>>,
    mut decompressed: &[u8],
    mut output: impl Write,
) -> std::io::Result<()> {
    let mut sum = 0;
    for command in commands {
        command.assert_length_valid();
        let (cmd_output, rest) = decompressed.split_at(command.decompressed_length().into());
        if cfg!(describe_costs) {
            sum += command.num_encoded_bytes();
            write!(
                output,
                "/* {:2} sum {sum:3} */",
                command.num_encoded_bytes()
            )?;
        }
        command.describe(cmd_output, &mut output)?;
        decompressed = rest;
    }

    assert_eq!(decompressed.len(), 0);
    writeln!(output, "\tlzend")
}

fn read_byte(mut input: impl Read) -> std::io::Result<u8> {
    let mut byte = 0;
    input
        .read_exact(std::slice::from_mut(&mut byte))
        .map(|()| byte)
}
fn deserialize<'data>(compressed: &'data [u8]) -> std::io::Result<Vec<Command<'data>>> {
    if compressed.is_empty() {
        return Err(eof_err("empty input"));
    }
    try_gb_len(compressed.len()); // TODO

    let mut commands = Vec::new();
    let mut rest = compressed;
    loop {
        let decompressed_thus_far = rest;
        let len_before_cmd = rest.len();

        let cmd = match LzCmdHeader::try_deserialize(&mut rest)? {
            Ok(LzCmdHeader {
                cmd_id: LzCmdId::Data,
                encoded_len,
            }) => Command::Data(LzData::deserialize(encoded_len, &mut rest)?),
            Ok(LzCmdHeader {
                cmd_id: LzCmdId::Repeat,
                encoded_len,
            }) => Command::Repeat(LzRepeat::deserialize(encoded_len, &mut rest)?),
            Ok(LzCmdHeader {
                cmd_id: LzCmdId::Alternate,
                encoded_len,
            }) => Command::Alternate(LzAlternate::deserialize(encoded_len, &mut rest)?),
            Ok(LzCmdHeader {
                cmd_id: LzCmdId::Zero,
                encoded_len,
            }) => Command::Zero(LzZero::deserialize(encoded_len, &mut rest)?),
            Ok(LzCmdHeader {
                cmd_id: LzCmdId::CopyNormal,
                encoded_len,
            }) => Command::CopyNormal(LzCopyNormal::deserialize(
                encoded_len,
                &mut rest,
                decompressed_thus_far,
            )?),
            Ok(LzCmdHeader {
                cmd_id: LzCmdId::CopyFlipped,
                encoded_len,
            }) => Command::CopyFlipped(LzCopyFlipped::deserialize(
                encoded_len,
                &mut rest,
                decompressed_thus_far,
            )?),
            Ok(LzCmdHeader {
                cmd_id: LzCmdId::CopyReversed,
                encoded_len,
            }) => Command::CopyReversed(LzCopyReversed::deserialize(
                encoded_len,
                &mut rest,
                decompressed_thus_far,
            )?),
            Err(LzSpecialCmdId::SpecialPackhi0) => {
                Command::SpecialPackhi0(LzSpecialPackhi0::deserialize(&mut rest)?)
            }
            Err(LzSpecialCmdId::SpecialPack16Short) => {
                Command::SpecialPack16Short(LzSpecialPack16Short::deserialize(&mut rest)?)
            }
            Err(LzSpecialCmdId::SpecialPacklo0) => {
                Command::SpecialPacklo0(LzSpecialPacklo0::deserialize(&mut rest)?)
            }
            Err(LzSpecialCmdId::End) => break,
        };

        assert_eq!(gb_len(len_before_cmd - rest.len()), cmd.num_encoded_bytes());
        commands.push(cmd);
    }

    if rest.is_empty() {
        Ok(commands)
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "trailing bytes found after end marker",
        ))
    }
}

// The behaviour of each LZ command is defined in a separate type,
// to try and keep each's behaviour definitions as local as possible.

trait LzCmd {
    /// Number of bytes that this command decompresses to.
    fn decompressed_length(&self) -> u16;
    /// This command refuses to encode fewer bytes than this.
    // Most commands are fine with encoding a single byte, but several have had their implementation
    // tweaked to unconditionally write more bytes than the length field encodes, because they would
    // otherwise be redundant with another command (e.g. `lzdata <byte>` is encoded using as many
    // bytes as `lzrepeat 1, <byte>` while emitting the same byte), and increasing the maximum
    // representable length allows a command to be more efficient at compressing data.
    const MIN_DECOMPRESSED_LENGTH: u16;
    /// This command cannot encode more bytes than this. (Inclusive.)
    const MAX_DECOMPRESSED_LENGTH: u16 =
        Self::MIN_DECOMPRESSED_LENGTH + LzCmdHeader::MAX_LONG_LENGTH - 1;

    /// How many bytes in the compressed output would this command take?
    fn num_encoded_bytes(&self) -> u16;
    fn describe(&self, bytes: &[u8], output: impl Write) -> std::io::Result<()>;

    // Helpers.

    fn decode_length(encoded_len: u16) -> u16 {
        assert!(encoded_len >= Self::MIN_DECOMPRESSED_LENGTH);
        assert!(encoded_len <= Self::MAX_DECOMPRESSED_LENGTH);
        encoded_len + Self::MIN_DECOMPRESSED_LENGTH
    }
    fn encoded_length(&self) -> u16 {
        self.decompressed_length() - Self::MIN_DECOMPRESSED_LENGTH
    }
    fn assert_length_valid(&self) {
        assert!(
            (Self::MIN_DECOMPRESSED_LENGTH..=Self::MAX_DECOMPRESSED_LENGTH)
                .contains(&self.decompressed_length()),
            "len {} not in {}..={}",
            self.decompressed_length(),
            Self::MIN_DECOMPRESSED_LENGTH,
            Self::MAX_DECOMPRESSED_LENGTH,
        );
    }
    fn make_header(&self, cmd_id: LzCmdId) -> LzCmdHeader {
        LzCmdHeader {
            cmd_id,
            encoded_len: self.encoded_length(),
        }
    }

    fn possible_data_lengths(decompressed_thus_far: &[u8]) -> RangeInclusive<u16> {
        Self::MIN_DECOMPRESSED_LENGTH
            ..=decompressed_thus_far
                .len()
                .min(Self::MAX_DECOMPRESSED_LENGTH.into()) // This *is* in u16 range...
                .try_into()
                .unwrap() // ...thus the smallest of the two *must* be in u16 range.
    }
}

// “Normal” command IDs.
#[derive(Debug, Clone, Copy)]
enum LzCmdId {
    Data,
    Repeat,
    Alternate,
    Zero,
    CopyNormal,
    CopyFlipped,
    CopyReversed,
}
impl TryFrom<u8> for LzCmdId {
    type Error = ();
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0b000 => Ok(Self::Data),
            0b001 => Ok(Self::Repeat),
            0b010 => Ok(Self::Alternate),
            0b011 => Ok(Self::Zero),
            0b100 => Ok(Self::CopyNormal),
            0b101 => Ok(Self::CopyFlipped),
            0b110 => Ok(Self::CopyReversed),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum LzSpecialCmdId {
    SpecialPackhi0 = 0xfc,
    SpecialPack16Short,
    SpecialPacklo0,
    End,
}
impl TryFrom<u8> for LzSpecialCmdId {
    type Error = ();
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0xfc => Ok(Self::SpecialPackhi0),
            0xfd => Ok(Self::SpecialPack16Short),
            0xfe => Ok(Self::SpecialPacklo0),
            0xff => Ok(Self::End),
            _ => Err(()),
        }
    }
}

/// Header of “normal” commands.
#[derive(Debug, Clone)]
struct LzCmdHeader {
    cmd_id: LzCmdId,
    encoded_len: u16,
}
impl LzCmdHeader {
    /// Maximum length the “short” header format can encode. (Exclusive.)
    const MAX_SHORT_LENGTH: u16 = 1 << 5;
    /// Maximum length the “long” header format can encode. (Exclusive.)
    const MAX_LONG_LENGTH: u16 = 1 << 9;

    fn num_encoded_bytes(&self) -> u16 {
        if self.encoded_len < Self::MAX_SHORT_LENGTH {
            1
        } else {
            2
        }
    }
    fn serialize(&self, mut output: impl Write) -> std::io::Result<()> {
        if self.encoded_len < Self::MAX_SHORT_LENGTH {
            output.write_all(&[(self.cmd_id as u8) << 5 | self.encoded_len as u8])
        } else {
            assert_matches!(self.encoded_len, ..Self::MAX_LONG_LENGTH);
            let [high, low] = self.encoded_len.to_be_bytes();
            output.write_all(&[0b111 << 5 | (self.cmd_id as u8) << 2 | high, low])
        }
    }

    fn try_deserialize(mut input: impl Read) -> std::io::Result<Result<Self, LzSpecialCmdId>> {
        let first_byte = read_byte(&mut input)?;
        Ok(if let Ok(cmd_id) = LzCmdId::try_from(first_byte >> 5) {
            Ok(Self {
                cmd_id,
                encoded_len: (first_byte & 0b1_1111).into(),
            })
        } else if let Ok(cmd_id) = LzCmdId::try_from((first_byte >> 2) % (1 << 3)) {
            let second_byte = read_byte(&mut input)?;
            Ok(Self {
                cmd_id,
                encoded_len: u16::from_be_bytes([first_byte, second_byte]) & 0b1_1111_1111,
            })
        } else {
            Err(first_byte.try_into().unwrap())
        })
    }
}

#[derive(Debug, Clone)]
struct LzData<'decompressed> {
    bytes: &'decompressed [u8],
}
impl<'decomp> LzData<'decomp> {
    fn for_each_possible_cmd(
        mut consider: impl FnMut(Command<'decomp>) -> bool,
        decompressed_thus_far: &'decomp [u8],
    ) {
        for length in Self::possible_data_lengths(decompressed_thus_far) {
            // Since this command's cost increases monotonically, if the cost becomes higher than the current best,
            // any further attempts are guaranteed to fail.
            if !consider(Command::Data(Self {
                bytes: last_n(decompressed_thus_far, length),
            })) {
                break;
            }
        }
    }

    /**
     * The command is followed by the bytes that it needs to copy (as many as its length, thus).
     */
    fn serialize(&self, mut output: impl Write) -> std::io::Result<()> {
        self.make_header(LzCmdId::Data).serialize(&mut output)?;
        output.write_all(self.bytes)
    }
    fn deserialize(encoded_len: u16, input: &mut &'decomp [u8]) -> std::io::Result<Self> {
        match input.split_at_checked(Self::decode_length(encoded_len).into()) {
            None => Err(eof_err("EOF while reading LzData bytes")),
            Some((bytes, rest)) => {
                *input = rest;
                Ok(Self { bytes })
            }
        }
    }

    fn decompress(&self, dest: &mut Vec<u8>) {
        dest.extend_from_slice(self.bytes);
    }
}
impl LzCmd for LzData<'_> {
    fn decompressed_length(&self) -> u16 {
        self.bytes.len().try_into().unwrap()
    }
    // This is the “fallback” command, and thus it needs to be able to emit a single byte.
    // (See also the comment on `LzRepeat::MIN_DECOMPRESSED_LENGTH` for more details of this decision.)
    const MIN_DECOMPRESSED_LENGTH: u16 = 1;

    fn num_encoded_bytes(&self) -> u16 {
        self.make_header(LzCmdId::Data).num_encoded_bytes() + gb_len(self.bytes.len())
    }
    fn describe(&self, bytes: &[u8], mut output: impl Write) -> std::io::Result<()> {
        assert_eq!(self.bytes, bytes);
        writeln!(output, "\tlzdata {}", Bytes(self.bytes))
    }
}

#[derive(Debug, Clone)]
struct LzRepeat {
    byte: u8,
    len: u16,
}
impl LzRepeat {
    fn header(&self) -> LzCmdHeader {
        self.make_header(LzCmdId::Repeat)
    }

    fn for_each_possible_cmd<'data>(
        mut consider: impl FnMut(Command<'data>) -> bool,
        decompressed_thus_far: &'data [u8],
    ) {
        let last_byte = *decompressed_thus_far.last().unwrap();
        // Any bytes between the start of `possible_data_lengths` and `last_byte` must also be repeats.
        if gb_len(decompressed_thus_far.len()) < Self::MIN_DECOMPRESSED_LENGTH
            || last_n(decompressed_thus_far, Self::MIN_DECOMPRESSED_LENGTH)
                .iter()
                .any(|&byte| byte != last_byte)
        {
            return;
        }
        for length in Self::possible_data_lengths(decompressed_thus_far) {
            // If the bytes are no longer repeating, stop.
            if rev_idx(decompressed_thus_far, length) != last_byte {
                break;
            }
            // TODO: this command's cost is actually constant.
            if !consider(Command::Repeat(Self {
                byte: last_byte,
                len: length,
            })) {
                break;
            }
        }
    }

    /**
     * The command is followed by the byte that ought to be repeated.
     */
    fn serialize(&self, mut output: impl Write) -> std::io::Result<()> {
        self.header().serialize(&mut output)?;
        output.write_all(&[self.byte])
    }
    fn deserialize(encoded_len: u16, input: &mut &'_ [u8]) -> std::io::Result<Self> {
        match input {
            [byte, rest @ ..] => {
                *input = rest;
                Ok(Self {
                    byte: *byte,
                    len: Self::decode_length(encoded_len),
                })
            }
            _ => Err(eof_err("EOF while reading LzRepeat byte")),
        }
    }

    fn decompress(&self, dest: &mut Vec<u8>) {
        dest.extend(std::iter::repeat_n(self.byte, self.len.into()));
    }
}
impl LzCmd for LzRepeat {
    fn decompressed_length(&self) -> u16 {
        self.len
    }
    // To avoid being equivalent to `LzData`.
    // Note that the minimum count of `LzData` could instead have been bumped to 2 instead, but
    // since that command performs no compression (it's pretty much a fall-back), we want to
    // generally avoid it, and thus it is not worth allowing it to be longer.
    const MIN_DECOMPRESSED_LENGTH: u16 = 2;

    fn num_encoded_bytes(&self) -> u16 {
        self.header().num_encoded_bytes() + 1
    }
    fn describe(&self, bytes: &[u8], mut output: impl Write) -> std::io::Result<()> {
        assert_eq!(bytes.iter().position(|&byte| byte != self.byte), None);
        writeln!(output, "\tlzrepeat {}, ${:02x}", self.len, self.byte)
    }
}

#[derive(Debug, Clone)]
struct LzAlternate {
    bytes: [u8; 2],
    len: u16,
}
impl LzAlternate {
    fn header(&self) -> LzCmdHeader {
        self.make_header(LzCmdId::Alternate)
    }

    fn for_each_possible_cmd<'data>(
        mut consider: impl FnMut(Command<'data>) -> bool,
        decompressed_thus_far: &'data [u8],
    ) {
        let Some(last_two) = decompressed_thus_far.last_chunk::<2>() else {
            return;
        };
        // Otherwise, there'll be a gap between the first two and the first one checked for repetition.
        assert_eq!(Self::MIN_DECOMPRESSED_LENGTH, 3);
        for length in Self::possible_data_lengths(decompressed_thus_far) {
            // If the bytes are no longer repeating alternated, stop.
            if rev_idx(decompressed_thus_far, length) != last_two[usize::from(length % 2)] {
                break;
            }
            // TODO: this command's cost is actually constant.
            if !consider(Command::Alternate(Self {
                // Careful that the first byte of this must be the one we *just* checked against!
                bytes: if length % 2 == 0 {
                    *last_two
                } else {
                    [last_two[1], last_two[0]]
                },
                len: length,
            })) {
                break;
            }
        }
    }

    /**
     * The command is followed by the two bytes that ought to be repeated.
     */
    fn serialize(&self, mut output: impl Write) -> std::io::Result<()> {
        self.header().serialize(&mut output)?;
        output.write_all(&self.bytes)
    }
    fn deserialize(encoded_len: u16, input: &mut &[u8]) -> std::io::Result<Self> {
        match input {
            [first, second, rest @ ..] => {
                *input = rest;
                Ok(Self {
                    bytes: [*first, *second],
                    len: Self::decode_length(encoded_len),
                })
            }
            _ => Err(eof_err("EOF while reading LzAlternate bytes")),
        }
    }

    fn decompress(&self, dest: &mut Vec<u8>) {
        dest.extend(
            std::iter::repeat(self.bytes)
                .flatten()
                .take(self.len.into()),
        );
    }
}
impl LzCmd for LzAlternate {
    fn decompressed_length(&self) -> u16 {
        self.len
    }
    // Less than this is just sparkling `LzData`. The same rationale as for `LzRepeat` applies.
    const MIN_DECOMPRESSED_LENGTH: u16 = 3;

    fn num_encoded_bytes(&self) -> u16 {
        self.header().num_encoded_bytes() + 2
    }
    fn describe(&self, bytes: &[u8], mut output: impl Write) -> std::io::Result<()> {
        assert_eq!(
            bytes
                .iter()
                .enumerate()
                .position(|(i, &byte)| byte != self.bytes[i % 2]),
            None
        );
        writeln!(
            output,
            "\tlzalternate {}, ${:02x}, ${:02x} ; {}",
            self.len,
            self.bytes[0],
            self.bytes[1],
            Bytes(bytes),
        )
    }
}

#[derive(Debug, Clone)]
struct LzZero {
    len: u16,
}
impl LzZero {
    fn header(&self) -> LzCmdHeader {
        self.make_header(LzCmdId::Zero)
    }

    fn for_each_possible_cmd<'data>(
        mut consider: impl FnMut(Command<'data>) -> bool,
        decompressed_thus_far: &'data [u8],
    ) {
        let last_byte = *decompressed_thus_far.last().unwrap();
        assert_eq!(last_byte, 0);
        // Any bytes between the start of `possible_data_lengths` and `last_byte` must also be repeats.
        if gb_len(decompressed_thus_far.len()) < Self::MIN_DECOMPRESSED_LENGTH
            || last_n(decompressed_thus_far, Self::MIN_DECOMPRESSED_LENGTH)
                .iter()
                .any(|&byte| byte != last_byte)
        {
            return;
        }
        for length in Self::possible_data_lengths(decompressed_thus_far) {
            // If the bytes are no longer repeating, stop.
            if rev_idx(decompressed_thus_far, length) != last_byte {
                break;
            }
            // TODO: this command's cost is actually constant.
            if !consider(Command::Zero(Self { len: length })) {
                break;
            }
        }
    }

    /**
     * The command does not have any payload bytes.
     */
    fn serialize(&self, mut output: impl Write) -> std::io::Result<()> {
        self.header().serialize(&mut output)
    }
    fn deserialize(encoded_len: u16, _input: &mut &[u8]) -> std::io::Result<Self> {
        Ok(Self {
            len: Self::decode_length(encoded_len),
        })
    }

    fn decompress(&self, dest: &mut Vec<u8>) {
        dest.extend(std::iter::repeat_n(0, self.len.into()));
    }
}
impl LzCmd for LzZero {
    fn decompressed_length(&self) -> u16 {
        self.len
    }
    // Note that this command uses the default minimum length of 1: unlike `LzRepeat`, `lzzero 1` is
    // *smaller* than `lzdata $00`! Thus, it's always worth attempting it when possible.
    const MIN_DECOMPRESSED_LENGTH: u16 = 1;

    fn num_encoded_bytes(&self) -> u16 {
        self.header().num_encoded_bytes()
    }
    fn describe(&self, bytes: &[u8], mut output: impl Write) -> std::io::Result<()> {
        assert_eq!(bytes.iter().position(|&byte| byte != 0), None);
        writeln!(output, "\tlzzero {}", self.len)
    }
}

// Since all of the “copy” commands behave mostly the same, most of their functionality is grouped here.
trait LzCopyCmd: LzCmd {
    /**
     * The command is followed by the offset of the first source byte in the decompressed stream;
     * it is encoded either as one negative byte, or as a positive big-endian word.
     * The former indicates that the offset is (127 - N) bytes behind the current write position,
     * the latter that the offset is N bytes from the start of the decompressed stream.
     */
    fn serialize(&self, mut output: impl Write) -> std::io::Result<()> {
        self.header().serialize(&mut output)?;
        self.start_ofs().serialize(output)
    }
    fn deserialize(
        encoded_len: u16,
        input: &mut &[u8],
        decompressed_thus_far: &[u8],
    ) -> std::io::Result<Self>
    where
        Self: Sized,
    {
        let start_ofs = LzCopyStartOfs::deserialize(input)?;
        // TODO: check that the start and end offsets are in range
        let start_idx = start_ofs.idx_within(decompressed_thus_far).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "copy start offset out of range",
            )
        })?;
        Self::new(Self::decode_length(encoded_len), start_ofs, start_idx).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "copy end offset is out of range",
            )
        })
    }

    fn decompress(&self, dest: &mut Vec<u8>);

    const CMD_ID: LzCmdId;
    fn start_ofs(&self) -> LzCopyStartOfs;
    fn new(len: u16, start_ofs: LzCopyStartOfs, start_idx: usize) -> Option<Self>
    where
        Self: Sized;
    fn header(&self) -> LzCmdHeader {
        self.make_header(Self::CMD_ID)
    }
}
/// Either negative 8-bit, or positive 15-bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LzCopyStartOfs(i16);
impl LzCopyStartOfs {
    fn new(src_end_idx: u16, copy_length: u16, dest_end_idx: u16) -> Self {
        // Since the copy's source and destination ranges are of equal length,
        // the offset between their start indices is equal to the offset between their end indices.
        Self::from_raw(src_end_idx - (copy_length - 1), dest_end_idx - src_end_idx)
    }
    fn new_reverse(src_end_idx: u16, copy_length: u16, dest_end_idx: u16) -> Self {
        // Since this copy's direction is backwards, we need to math differently.
        let src_start_idx = src_end_idx + (copy_length - 1);
        let dest_start_idx = dest_end_idx - (copy_length - 1);
        Self::from_raw(src_start_idx, dest_start_idx - src_start_idx)
    }
    fn from_raw(src_start_idx: u16, start_ofs_from_end: u16) -> Self {
        match start_ofs_from_end {
            ofs_from_end @ 1..=128 => Self(-(ofs_from_end as i16)),
            0 => unreachable!(),
            // `gb_len` should guarantee that this cannot fail.
            _ => Self(src_start_idx.try_into().unwrap()),
        }
    }

    fn num_encoded_bytes(&self) -> u16 {
        if self.0 < 0 { 1 } else { 2 }
    }
    fn serialize(&self, mut output: impl Write) -> std::io::Result<()> {
        if self.0 < 0 {
            output.write_all(&[(127 - self.0) as u8])
        } else {
            // MSB first, since bit 7 of the first byte is what selects between the two variants.
            output.write_all(&self.0.to_be_bytes())
        }
    }
    fn deserialize(bytes: &mut &[u8]) -> std::io::Result<Self> {
        match bytes {
            [] => Err(eof_err("EOF found instead of LZ copy command start offset")),

            [rel_ofs @ 128..=255, rest @ ..] => {
                *bytes = rest;
                Ok(Self((*rel_ofs as i8).into()))
            }

            [abs_ofs_hi @ 0..=127, abs_ofs_lo, rest @ ..] => {
                *bytes = rest;
                Ok(Self(i16::from_be_bytes([*abs_ofs_hi, *abs_ofs_lo])))
            }
            [0..=127] => Err(eof_err(
                "EOF found in the middle of two-byte LZ copy command start offset",
            )),
        }
    }

    fn idx_within(&self, slice: &[u8]) -> Option<usize> {
        if self.0 < 0 {
            slice.len().checked_add_signed(self.0.into())
        } else {
            let start_idx = self.0 as usize;
            (start_idx < slice.len()).then_some(start_idx)
        }
    }
}
fn for_each_possible_copy_cmd<'data>(
    mut consider: impl FnMut(Command<'data>) -> bool,
    decompressed_thus_far: &[u8],
) {
    let last_byte = *decompressed_thus_far.last().unwrap();
    let last_byte_flipped = last_byte.reverse_bits();
    // Seek opportunities for referencing previously-decompressed data;
    // since, as usual, we are looking for copies that *end* at the current byte,
    // we anchor searches based on that byte, and then proceed through each candidate range in
    // the direction *opposite* the copy's normal direction!

    let dest_end_idx = gb_len(decompressed_thus_far.len() - 1);
    // Make sure to search for anchors backwards, so commands with smaller encodings are attempted first,
    // and the “cost short-circuiting” optimisation remains valid.
    // Also, start searching *before* `last_byte`, otherwise we would be trying to copy the whole
    // data onto itself.
    for src_end_idx in (0..dest_end_idx).rev() {
        if decompressed_thus_far[usize::from(src_end_idx)] == last_byte {
            // Check if a normal, forward copy is possible.
            let source_range = &decompressed_thus_far[..=usize::from(src_end_idx)];
            for length in LzCopyNormal::possible_data_lengths(source_range) {
                // If the copy doesn't match anymore, stop.
                // It is okay for these two ranges to overlap, though!
                // The first few non-overlapping bytes then get re-read, and implicitly repeated.
                if rev_idx(decompressed_thus_far, length) != rev_idx(source_range, length) {
                    break;
                }
                let start_ofs = LzCopyStartOfs::new(src_end_idx, length, dest_end_idx);
                // Since this command's cost increases monotonically (though in a single step),
                // if the cost becomes higher than the current best,
                // any further attempts are guaranteed to fail.
                if !consider(Command::CopyNormal(LzCopyNormal {
                    len: length,
                    start_ofs,
                })) {
                    break;
                }
            }

            // Try also a reverse-direction copy; the logic is the same,
            // but `src` grows to the *right* instead.
            let active_bytes = &decompressed_thus_far[usize::from(src_end_idx)..]; // Bytes outside this will be neither read nor written.
            // Unlike a forward copy, these ranges must NOT overlap, since they grow in opposite directions.
            // `active_bytes` will be consumed from the left by the source, and from the right by the destination
            // (keep in mind that we are staring from each's respective end, so we are thinking backwards here),
            // thus we need to halve the range of `active_bytes`.
            let source_range = &active_bytes[..active_bytes.len() / 2]; // Intentionally rounding down.
            for length in LzCopyReversed::possible_data_lengths(source_range) {
                // If the copy doesn't match anymore, stop.
                if rev_idx(decompressed_thus_far, length) != source_range[usize::from(length) - 1] {
                    break;
                }
                let start_ofs = LzCopyStartOfs::new_reverse(src_end_idx, length, dest_end_idx);
                // Since this command's cost increases monotonically (though in a single step),
                // if the cost becomes higher than the current best,
                // any further attempts are guaranteed to fail.
                if !consider(Command::CopyReversed(LzCopyReversed {
                    len: length,
                    start_ofs,
                })) {
                    break;
                }
            }
        }
        // This is not mutually exclusive with the above, in case the byte is symmetrical.
        if decompressed_thus_far[usize::from(src_end_idx)] == last_byte_flipped {
            // Check if a normal, forward copy is possible.
            let source_range = &decompressed_thus_far[..=usize::from(src_end_idx)];
            for length in LzCopyFlipped::possible_data_lengths(source_range) {
                // If the copy doesn't match anymore, stop.
                // It is okay for these two ranges to overlap, though!
                // The first few non-overlapping bytes then get re-read, and implicitly repeated (but flipped again!).
                if rev_idx(decompressed_thus_far, length)
                    != rev_idx(source_range, length).reverse_bits()
                {
                    break;
                }
                let start_ofs = LzCopyStartOfs::new(src_end_idx, length, dest_end_idx);
                // Since this command's cost increases monotonically (though in a single step),
                // if the cost becomes higher than the current best,
                // any further attempts are guaranteed to fail.
                if !consider(Command::CopyFlipped(LzCopyFlipped {
                    len: length,
                    start_ofs,
                })) {
                    break;
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
struct LzCopyNormal {
    len: u16,
    start_ofs: LzCopyStartOfs,
}
impl LzCmd for LzCopyNormal {
    fn decompressed_length(&self) -> u16 {
        self.len
    }

    // FIXME: due to the overhead of the offset, this should be at least 2.
    //        This may be feasible to implement by unconditionally copying the first byte before
    //        entering the appropriate copy loop (with an ad-hoc correction for the “flipped”
    //        variant), and incrementing/decrementing the source *before* each copy within the loop.
    //        (Make sure to change `for_each_possible_cmd` accordingly!)
    const MIN_DECOMPRESSED_LENGTH: u16 = 1;

    fn num_encoded_bytes(&self) -> u16 {
        self.make_header(LzCmdId::CopyNormal).num_encoded_bytes()
            + self.start_ofs.num_encoded_bytes()
    }
    fn describe(&self, bytes: &[u8], mut output: impl Write) -> std::io::Result<()> {
        writeln!(
            output,
            "\tlzcopy normal, {}, {} ; {}",
            self.len,
            self.start_ofs.0,
            Bytes(bytes),
        )
    }
}
impl LzCopyCmd for LzCopyNormal {
    fn decompress(&self, dest: &mut Vec<u8>) {
        let start_idx = self.start_ofs.idx_within(dest).unwrap();
        dest.extend_from_within(start_idx..start_idx + usize::from(self.len));
    }

    const CMD_ID: LzCmdId = LzCmdId::CopyNormal;
    fn start_ofs(&self) -> LzCopyStartOfs {
        self.start_ofs
    }
    fn new(encoded_len: u16, start_ofs: LzCopyStartOfs, _copy_start_idx: usize) -> Option<Self> {
        // If the copy's source writes past the end of the already-decompressed data,
        // it will merely read the already-decompressed data, which is fine (and useful!).
        Some(Self {
            len: Self::decode_length(encoded_len),
            start_ofs,
        })
    }
}

#[derive(Debug, Clone)]
struct LzCopyFlipped {
    len: u16,
    start_ofs: LzCopyStartOfs,
}
impl LzCmd for LzCopyFlipped {
    fn decompressed_length(&self) -> u16 {
        self.len
    }

    // FIXME: due to the overhead of the offset, this should be at least 2.
    //        This may be feasible to implement by unconditionally copying the first byte before
    //        entering the appropriate copy loop (with an ad-hoc correction for the “flipped”
    //        variant), and incrementing/decrementing the source *before* each copy within the loop.
    //        (Make sure to change `for_each_possible_cmd` accordingly!)
    const MIN_DECOMPRESSED_LENGTH: u16 = 1;

    fn num_encoded_bytes(&self) -> u16 {
        self.make_header(LzCmdId::CopyNormal).num_encoded_bytes()
            + self.start_ofs.num_encoded_bytes()
    }
    fn describe(&self, bytes: &[u8], mut output: impl Write) -> std::io::Result<()> {
        writeln!(
            output,
            "\tlzcopy flipped, {}, {} ; {}",
            self.len,
            self.start_ofs.0,
            Bytes(bytes),
        )
    }
}
impl LzCopyCmd for LzCopyFlipped {
    fn decompress(&self, dest: &mut Vec<u8>) {
        let mut start_idx = self.start_ofs.idx_within(dest).unwrap();
        for _ in 0..self.len {
            dest.push(dest[start_idx].reverse_bits());
            start_idx += 1;
        }
    }

    const CMD_ID: LzCmdId = LzCmdId::CopyFlipped;
    fn start_ofs(&self) -> LzCopyStartOfs {
        self.start_ofs
    }
    fn new(encoded_len: u16, start_ofs: LzCopyStartOfs, _copy_start_idx: usize) -> Option<Self> {
        // If the copy's source writes past the end of the already-decompressed data,
        // it will merely read the already-decompressed data, which is fine (and useful!).
        Some(Self {
            len: Self::decode_length(encoded_len),
            start_ofs,
        })
    }
}

#[derive(Debug, Clone)]
struct LzCopyReversed {
    len: u16,
    start_ofs: LzCopyStartOfs,
}
impl LzCmd for LzCopyReversed {
    fn decompressed_length(&self) -> u16 {
        self.len
    }

    // FIXME: due to the overhead of the offset, this should be at least 2.
    //        This may be feasible to implement by unconditionally copying the first byte before
    //        entering the appropriate copy loop (with an ad-hoc correction for the “flipped”
    //        variant), and incrementing/decrementing the source *before* each copy within the loop.
    //        (Make sure to change `for_each_possible_cmd` accordingly!)
    const MIN_DECOMPRESSED_LENGTH: u16 = 1;

    fn num_encoded_bytes(&self) -> u16 {
        self.make_header(LzCmdId::CopyNormal).num_encoded_bytes()
            + self.start_ofs.num_encoded_bytes()
    }
    fn describe(&self, bytes: &[u8], mut output: impl Write) -> std::io::Result<()> {
        writeln!(
            output,
            "\tlzcopy reversed, {}, {} ; {}",
            self.len,
            self.start_ofs.0,
            Bytes(bytes),
        )
    }
}
impl LzCopyCmd for LzCopyReversed {
    fn decompress(&self, dest: &mut Vec<u8>) {
        let mut start_idx = self.start_ofs.idx_within(dest).unwrap();
        for _ in 0..self.len {
            dest.push(dest[start_idx]);
            start_idx -= 1;
        }
    }

    const CMD_ID: LzCmdId = LzCmdId::CopyReversed;
    fn start_ofs(&self) -> LzCopyStartOfs {
        self.start_ofs
    }
    fn new(encoded_len: u16, start_ofs: LzCopyStartOfs, copy_start_idx: usize) -> Option<Self> {
        let len = Self::decode_length(encoded_len);
        copy_start_idx
            .checked_sub((len - 1).into())
            .map(|_end_idx| Self { len, start_ofs })
    }
}

/**
 * All special commands use a different header format:
 * - the first byte encodes the command ID (remember that the encoding of normal command forces
 *   its upper 6 bits to all be 1),
 * - the second byte encodes the command's length.
 *
 * Then, each output byte is encoded as a single nibble, which are then packed into bytes (high nibble first).
 */
trait LzSpecialCmd<'data>: LzCmd {
    const CMD_ID: LzSpecialCmdId;
    const DESCRIBING_NAME: &str;

    fn compressed_nibbles(&self) -> impl ExactSizeIterator<Item = u8>;
    fn serialize(&self, mut output: impl Write) -> std::io::Result<()> {
        let mut nibbles = self.compressed_nibbles();

        output.write_all(&[
            Self::CMD_ID as u8,
            (nibbles.len() - usize::from(Self::MIN_DECOMPRESSED_LENGTH))
                .try_into()
                .unwrap(),
        ])?;

        while let Some(upper_nibble) = nibbles.next() {
            assert_eq!(upper_nibble & 0xf0, 0);
            let lower_nibble = nibbles.next().unwrap_or_default();
            assert_eq!(lower_nibble & 0xf0, 0);
            output.write_all(&[upper_nibble << 4 | lower_nibble])?;
        }
        Ok(())
    }

    fn decode(nibbles: impl Iterator<Item = u8>) -> Self;
    fn deserialize(bytes: &mut &[u8]) -> std::io::Result<Self>
    where
        Self: Sized,
    {
        let (encoded_len, after_len) = bytes
            .split_first()
            .ok_or_else(|| eof_err("EOF found instead of special command's length"))?;
        let num_nibbles = Self::decode_length((*encoded_len).into());
        let (payload, rest) = after_len
            .split_at_checked(num_bytes_to_encode_nibbles(num_nibbles).into())
            .ok_or_else(|| eof_err("EOF while reading special command's payload"))?;
        *bytes = rest;
        Ok(Self::decode(
            payload
                .iter()
                .flat_map(|&byte| [byte >> 4, byte & 0x0f])
                .take(num_nibbles.into()),
        ))
    }

    fn decompress(&self, dest: &mut Vec<u8>);

    fn make_cmd(decompressed: &'data [u8]) -> Command<'data>;
    fn can_encode(byte: u8) -> bool;
    fn for_each_possible_cmd(
        mut consider: impl FnMut(Command<'data>) -> bool,
        decompressed_thus_far: &'data [u8],
    ) {
        // Check that all bytes before the smallest length are also encodable.
        if gb_len(decompressed_thus_far.len()) < Self::MIN_DECOMPRESSED_LENGTH
            || last_n(decompressed_thus_far, Self::MIN_DECOMPRESSED_LENGTH)
                .iter()
                .any(|&byte| !Self::can_encode(byte))
        {
            return;
        }
        for length in Self::possible_data_lengths(decompressed_thus_far) {
            if !Self::can_encode(rev_idx(decompressed_thus_far, length)) {
                break;
            }
            // Since this command's cost increases monotonically, if the cost is higher than the
            // current best sum, any further attempts are guaranteed to fail.
            if !consider(Self::make_cmd(last_n(decompressed_thus_far, length))) {
                break;
            }
        }
    }
}
impl<'data, T: LzSpecialCmd<'data>> LzCmd for T {
    fn decompressed_length(&self) -> u16 {
        self.compressed_nibbles().len().try_into().unwrap()
    }

    // FIXME: min length should be 2, and then the max length should become 257,
    //        or that value reserved for some special purpose...
    const MIN_DECOMPRESSED_LENGTH: u16 = 1;
    // All of the special commands use an 8-bit length instead of the regular header encoding.
    const MAX_DECOMPRESSED_LENGTH: u16 = Self::MIN_DECOMPRESSED_LENGTH + 256 - 1;

    fn num_encoded_bytes(&self) -> u16 {
        1 + 1 + num_bytes_to_encode_nibbles(self.decompressed_length())
    }
    fn describe(&self, bytes: &[u8], mut output: impl Write) -> std::io::Result<()> {
        writeln!(output, "\t{} {}", Self::DESCRIBING_NAME, Bytes(bytes))
    }
}
fn num_bytes_to_encode_nibbles(num_nibbles: u16) -> u16 {
    // If the number of nibbles is odd, that requires an entire byte! So, we round up.
    num_nibbles.div_ceil(2)
}

#[derive(Debug, Clone)]
struct LzSpecialPackhi0<'decompressed> {
    bytes: Cow<'decompressed, [u8]>,
}
impl<'data> LzSpecialCmd<'data> for LzSpecialPackhi0<'data> {
    const CMD_ID: LzSpecialCmdId = LzSpecialCmdId::SpecialPackhi0;
    const DESCRIBING_NAME: &str = "lzpackhi";

    // This command is basically `LzData` but with all low nibbles equal to 0.
    fn compressed_nibbles(&self) -> impl ExactSizeIterator<Item = u8> {
        self.bytes.iter().map(|byte| {
            assert_eq!(byte & 0x0f, 0);
            byte >> 4
        })
    }
    fn decode(nibbles: impl Iterator<Item = u8>) -> Self {
        Self {
            bytes: nibbles.map(|nibble| nibble << 4).collect(),
        }
    }
    fn decompress(&self, dest: &mut Vec<u8>) {
        dest.extend_from_slice(&self.bytes);
    }

    fn make_cmd(decompressed: &'data [u8]) -> Command<'data> {
        Command::SpecialPackhi0(Self {
            bytes: decompressed.into(),
        })
    }
    fn can_encode(byte: u8) -> bool {
        byte & 0x0f == 0
    }
}

#[derive(Debug, Clone)]
struct LzSpecialPack16Short<'decompressed> {
    bytes: Cow<'decompressed, [u8]>,
}
impl LzSpecialPack16Short<'_> {
    const DICT: [u8; 16] = [
        0x00, 0xff, 0x01, 0x02, 0x03, 0xfe, 0x80, 0x07, 0xc0, 0x7f, 0x04, 0x0f, 0x1f, 0x3f, 0x08,
        0xfc,
    ];
}
impl<'data> LzSpecialCmd<'data> for LzSpecialPack16Short<'data> {
    const CMD_ID: LzSpecialCmdId = LzSpecialCmdId::SpecialPack16Short;
    const DESCRIBING_NAME: &str = "lzpack16";

    fn compressed_nibbles(&self) -> impl ExactSizeIterator<Item = u8> {
        self.bytes
            .iter()
            .map(|&byte| Self::DICT.iter().position(|&entry| entry == byte).unwrap() as u8)
    }
    fn decode(nibbles: impl Iterator<Item = u8>) -> Self {
        Self {
            bytes: nibbles
                .map(|nibble| Self::DICT[usize::from(nibble)])
                .collect(),
        }
    }
    fn decompress(&self, dest: &mut Vec<u8>) {
        dest.extend_from_slice(&self.bytes);
    }

    fn make_cmd(decompressed: &'data [u8]) -> Command<'data> {
        Command::SpecialPack16Short(Self {
            bytes: decompressed.into(),
        })
    }
    fn can_encode(byte: u8) -> bool {
        Self::DICT.contains(&byte)
    }
}

#[derive(Debug, Clone)]
struct LzSpecialPacklo0<'decompressed> {
    bytes: Cow<'decompressed, [u8]>,
}
impl<'data> LzSpecialCmd<'data> for LzSpecialPacklo0<'data> {
    const CMD_ID: LzSpecialCmdId = LzSpecialCmdId::SpecialPacklo0;
    const DESCRIBING_NAME: &str = "lzpacklo";

    fn compressed_nibbles(&self) -> impl ExactSizeIterator<Item = u8> {
        self.bytes.iter().map(|&byte| {
            assert_eq!(byte & 0xf0, 0);
            byte
        })
    }
    fn decode(nibbles: impl Iterator<Item = u8>) -> Self {
        Self {
            bytes: nibbles.collect(),
        }
    }
    fn decompress(&self, dest: &mut Vec<u8>) {
        dest.extend_from_slice(&self.bytes);
    }

    fn make_cmd(decompressed: &'data [u8]) -> Command<'data> {
        Command::SpecialPacklo0(Self {
            bytes: decompressed.into(),
        })
    }
    fn can_encode(byte: u8) -> bool {
        byte & 0xf0 == 0
    }
}

// Helpers for various `for_each_possible_cmd` functions.
/// Returns the `idx`th (1-indexed!) byte from the *end* of `slice`.
fn rev_idx(slice: &[u8], idx: u16) -> u8 {
    slice[slice.len() - usize::from(idx)]
}
/// Returns the last `len` bytes from the *end* of the slice.
fn last_n(slice: &[u8], len: u16) -> &[u8] {
    &slice[slice.len() - usize::from(len)..]
}

// Helper for `LzCmd::describe()` impls.
struct Bytes<'a>(&'a [u8]);
impl std::fmt::Display for Bytes<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Assuming the slice isn't empty.
        write!(f, "${:02x}", self.0[0])?;
        for byte in &self.0[1..] {
            write!(f, ", ${byte:02x}")?;
        }
        Ok(())
    }
}

// Helper for various deserialisation functions.
fn eof_err(msg: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::UnexpectedEof, msg)
}

// CLI argument parsing.

impl Options {
    fn parse_cli() -> Self {
        let mut options = Options {
            input: None,
            output: None,
            mode: OperationMode::Compress(OutputMode::Binary),
            alignment_nb_bits: 0,
        };

        let mut args = std::env::args();
        let program_name = args.next().unwrap_or("lzpcompress".into());

        let mut accepting_options = true;
        while let Some(arg) = args.next() {
            if accepting_options
                && let Some(rest) = arg.strip_prefix('-')
                // "-" is a positional arg, not an option.
                && !rest.is_empty()
            {
                if let Some(rest) = rest.strip_prefix('-') {
                    // Long option.
                    let (long_opt_name, long_opt_arg) = match rest.split_once('=') {
                        Some((name, arg)) => (name, Some(Cow::Borrowed(arg))), // `--opt=arg`
                        None => (rest, None),
                    };
                    let ensure_no_arg = || {
                        if long_opt_arg.is_some() {
                            eprintln!("error: --{long_opt_name} does not take an argument");
                            std::process::exit(EXIT_BAD_USAGE);
                        }
                    };
                    let require_arg = || {
                        args.next()
                            .unwrap_or_else(|| {
                                eprintln!("error: missing argument to --{long_opt_name}");
                                std::process::exit(EXIT_BAD_USAGE);
                            })
                            .into()
                    };

                    match long_opt_name {
                        "" => {
                            // "--"
                            ensure_no_arg();
                            accepting_options = false;
                        }
                        "help" => {
                            ensure_no_arg();
                            usage(EXIT_SUCCESS, &program_name);
                        }
                        "binary" => {
                            ensure_no_arg();
                            options.mode = OperationMode::Compress(OutputMode::Binary);
                        }
                        "text" => {
                            ensure_no_arg();
                            options.mode = OperationMode::Compress(OutputMode::Textual);
                        }
                        "uncompress" | "decompress" => {
                            ensure_no_arg();
                            options.mode = OperationMode::Decompress(OutputMode::Binary);
                        }
                        "dump" => {
                            ensure_no_arg();
                            options.mode = OperationMode::Decompress(OutputMode::Textual);
                        }
                        "align" => {
                            let arg = long_opt_arg.unwrap_or_else(require_arg);
                            match Self::parse_alignment_arg(&arg) {
                                Ok(alignment) => options.alignment_nb_bits = alignment,
                                Err(kind) => {
                                    eprintln!(
                                        "error: argument to --{long_opt_name} {}",
                                        Self::alignment_arg_error(kind),
                                    );
                                    std::process::exit(EXIT_BAD_USAGE);
                                }
                            }
                        }
                        _ => {
                            eprintln!("error: unknown option --{long_opt_name}");
                            std::process::exit(EXIT_BAD_USAGE);
                        }
                    }
                } else {
                    // Short option.
                    // Since short options can be packed into a single arg, parse it char by char.
                    // However, `-a10`, for example, is equivalent to `-a 10`!
                    let mut arg_chars = rest.chars();
                    while let Some(short_opt_name) = arg_chars.next() {
                        let mut require_arg = || match arg_chars.as_str() {
                            "" => args
                                .next()
                                .unwrap_or_else(|| {
                                    eprintln!("error: missing argument to -{short_opt_name}");
                                    std::process::exit(EXIT_BAD_USAGE);
                                })
                                .into(),
                            arg => Cow::Borrowed(arg),
                        };

                        match short_opt_name {
                            '?' | 'h' => usage(EXIT_SUCCESS, &program_name),
                            'b' => options.mode = OperationMode::Compress(OutputMode::Binary),
                            't' => options.mode = OperationMode::Compress(OutputMode::Textual),
                            'u' => options.mode = OperationMode::Decompress(OutputMode::Binary),
                            'd' => options.mode = OperationMode::Decompress(OutputMode::Textual),
                            'a' => {
                                let arg = require_arg();
                                match Self::parse_alignment_arg(&arg) {
                                    Ok(alignment) => options.alignment_nb_bits = alignment,
                                    Err(kind) => {
                                        eprintln!(
                                            "error: argument to -{short_opt_name} {}",
                                            Self::alignment_arg_error(kind),
                                        );
                                        std::process::exit(EXIT_BAD_USAGE);
                                    }
                                }
                                break; // This (implicitly) consumes the rest of `arg_chars`.
                            }
                            _ => {
                                eprintln!("error: unknown option -{short_opt_name}");
                                std::process::exit(EXIT_BAD_USAGE);
                            }
                        }
                    }
                }
            } else {
                // Positional argument.
                if options.input.is_none() {
                    options.input = Some(arg.into());
                } else if options.output.is_none() {
                    options.output = Some(arg.into());
                } else {
                    eprintln!("error: too many positional arguments (use `--help`)");
                    std::process::exit(EXIT_BAD_USAGE);
                }
            }
        }

        options
    }

    const MAX_ALIGNMENT: u8 = 12;
    fn parse_alignment_arg(arg: &str) -> Result<u8, IntErrorKind> {
        match arg.parse() {
            Err(err) => Err(*err.kind()),
            Ok(value @ 0..=Self::MAX_ALIGNMENT) => Ok(value),
            Ok(_) => Err(IntErrorKind::PosOverflow),
        }
    }
    fn alignment_arg_error(kind: IntErrorKind) -> &'static str {
        match kind {
            IntErrorKind::Empty => "cannot be empty",
            IntErrorKind::InvalidDigit => "must be a number",
            IntErrorKind::PosOverflow => "must be at most 12",
            _ => unreachable!(),
        }
    }

    fn path_arg(arg: Option<&Path>) -> Option<&Path> {
        arg.and_then(|path| (path != "-").then_some(path))
    }
}

enum OutputFile<'stdout> {
    File(File),
    Stdout(std::io::StdoutLock<'stdout>),
}
impl std::io::Write for OutputFile<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::File(file) => file.write(buf),
            Self::Stdout(stdout) => stdout.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::File(file) => file.flush(),
            Self::Stdout(stdout) => stdout.flush(),
        }
    }
}
