//! This module relocates a BPF ELF

// Note: Typically ELF shared objects are loaded using the program headers and
// not the section headers.  Since we are leveraging the elfkit crate its much
// easier to use the section headers.  There are cases (reduced size, obfuscation)
// where the section headers may be removed from the ELF.  If that happens then
// this loader will need to be re-written to use the program headers instead.

use crate::{
    aligned_memory::{is_memory_aligned, AlignedMemory},
    ebpf::{self, EF_SBPF_V2, HOST_ALIGN, INSN_SIZE},
    elf_parser::{
        consts::{
            ELFCLASS64, ELFDATA2LSB, ELFOSABI_NONE, EM_BPF, EM_SBPF, ET_DYN, R_X86_64_32,
            R_X86_64_64, R_X86_64_NONE, R_X86_64_RELATIVE,
        },
        types::{Elf64Phdr, Elf64Shdr, Elf64Word},
        Elf64, ElfParserError,
    },
    error::EbpfError,
    memory_region::MemoryRegion,
    program::{BuiltinProgram, FunctionRegistry, SBPFVersion},
    verifier::Verifier,
    vm::{Config, ContextObject},
};

#[cfg(all(feature = "jit", not(target_os = "windows"), target_arch = "x86_64"))]
use crate::jit::{JitCompiler, JitProgram};
use byteorder::{ByteOrder, LittleEndian};
use std::{collections::BTreeMap, fmt::Debug, mem, ops::Range, str};

#[cfg(not(feature = "shuttle-test"))]
use std::sync::Arc;

#[cfg(feature = "shuttle-test")]
use shuttle::sync::Arc;

/// Error definitions
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ElfError {
    /// Failed to parse ELF file
    #[error("Failed to parse ELF file: {0}")]
    FailedToParse(String),
    /// Entrypoint out of bounds
    #[error("Entrypoint out of bounds")]
    EntrypointOutOfBounds,
    /// Invalid entrypoint
    #[error("Invalid entrypoint")]
    InvalidEntrypoint,
    /// Failed to get section
    #[error("Failed to get section {0}")]
    FailedToGetSection(String),
    /// Unresolved symbol
    #[error("Unresolved symbol ({0}) at instruction #{1:?} (ELF file offset {2:#x})")]
    UnresolvedSymbol(String, usize, usize),
    /// Section not found
    #[error("Section not found: {0}")]
    SectionNotFound(String),
    /// Relative jump out of bounds
    #[error("Relative jump out of bounds at instruction #{0}")]
    RelativeJumpOutOfBounds(usize),
    /// Symbol hash collision
    #[error("Symbol hash collision {0:#x}")]
    SymbolHashCollision(u32),
    /// Incompatible ELF: wrong endianess
    #[error("Incompatible ELF: wrong endianess")]
    WrongEndianess,
    /// Incompatible ELF: wrong ABI
    #[error("Incompatible ELF: wrong ABI")]
    WrongAbi,
    /// Incompatible ELF: wrong machine
    #[error("Incompatible ELF: wrong machine")]
    WrongMachine,
    /// Incompatible ELF: wrong class
    #[error("Incompatible ELF: wrong class")]
    WrongClass,
    /// Not one text section
    #[error("Multiple or no text sections, consider removing llc option: -function-sections")]
    NotOneTextSection,
    /// Read-write data not supported
    #[error("Found writable section ({0}) in ELF, read-write data not supported")]
    WritableSectionNotSupported(String),
    /// Relocation failed, no loadable section contains virtual address
    #[error("Relocation failed, no loadable section contains virtual address {0:#x}")]
    AddressOutsideLoadableSection(u64),
    /// Relocation failed, invalid referenced virtual address
    #[error("Relocation failed, invalid referenced virtual address {0:#x}")]
    InvalidVirtualAddress(u64),
    /// Relocation failed, unknown type
    #[error("Relocation failed, unknown type {0:?}")]
    UnknownRelocation(u32),
    /// Failed to read relocation info
    #[error("Failed to read relocation info")]
    FailedToReadRelocationInfo,
    /// Incompatible ELF: wrong type
    #[error("Incompatible ELF: wrong type")]
    WrongType,
    /// Unknown symbol
    #[error("Unknown symbol with index {0}")]
    UnknownSymbol(usize),
    /// Offset or value is out of bounds
    #[error("Offset or value is out of bounds")]
    ValueOutOfBounds,
    /// Detected sbpf_version required by the executable which are not enabled
    #[error("Detected sbpf_version required by the executable which are not enabled")]
    UnsupportedSBPFVersion,
    /// Invalid program header
    #[error("Invalid ELF program header")]
    InvalidProgramHeader,
}

impl From<ElfParserError> for ElfError {
    fn from(err: ElfParserError) -> Self {
        match err {
            ElfParserError::InvalidSectionHeader
            | ElfParserError::InvalidString
            | ElfParserError::InvalidSize
            | ElfParserError::Overlap
            | ElfParserError::SectionNotInOrder
            | ElfParserError::NoSectionNameStringTable
            | ElfParserError::InvalidDynamicSectionTable
            | ElfParserError::InvalidRelocationTable
            | ElfParserError::InvalidAlignment
            | ElfParserError::NoStringTable
            | ElfParserError::NoDynamicStringTable
            | ElfParserError::InvalidFileHeader
            | ElfParserError::StringTooLong(_, _) => ElfError::FailedToParse(err.to_string()),
            ElfParserError::InvalidProgramHeader => ElfError::InvalidProgramHeader,
            ElfParserError::OutOfBounds => ElfError::ValueOutOfBounds,
        }
    }
}

fn get_section(elf: &Elf64, name: &[u8]) -> Result<Elf64Shdr, ElfError> {
    for section_header in elf.section_header_table() {
        if elf.section_name(section_header.sh_name)? == name {
            return Ok(section_header.clone());
        }
    }

    Err(ElfError::SectionNotFound(
        std::str::from_utf8(name)
            .unwrap_or("UTF-8 error")
            .to_string(),
    ))
}

// For more information on the BPF instruction set:
// https://github.com/iovisor/bpf-docs/blob/master/eBPF.md

// msb                                                        lsb
// +------------------------+----------------+----+----+--------+
// |immediate               |offset          |src |dst |opcode  |
// +------------------------+----------------+----+----+--------+

// From least significant to most significant bit:
//   8 bit opcode
//   4 bit destination register (dst)
//   4 bit source register (src)
//   16 bit offset
//   32 bit immediate (imm)

/// Byte offset of the immediate field in the instruction
const BYTE_OFFSET_IMMEDIATE: usize = 4;
/// Byte length of the immediate field
const BYTE_LENGTH_IMMEDIATE: usize = 4;

/// BPF relocation types.
#[allow(non_camel_case_types)]
#[derive(Debug, PartialEq, Copy, Clone)]
enum BpfRelocationType {
    /// No relocation, placeholder
    R_Bpf_None = 0,
    /// R_BPF_64_64 relocation type is used for ld_imm64 instruction.
    /// The actual to-be-relocated data (0 or section offset) is
    /// stored at r_offset + 4 and the read/write data bitsize is 32
    /// (4 bytes). The relocation can be resolved with the symbol
    /// value plus implicit addend.
    R_Bpf_64_64 = 1,
    /// 64 bit relocation of a ldxdw instruction.  The ldxdw
    /// instruction occupies two instruction slots. The 64-bit address
    /// to load from is split into the 32-bit imm field of each
    /// slot. The first slot's pre-relocation imm field contains the
    /// virtual address (typically same as the file offset) of the
    /// location to load. Relocation involves calculating the
    /// post-load 64-bit physical address referenced by the imm field
    /// and writing that physical address back into the imm fields of
    /// the ldxdw instruction.
    R_Bpf_64_Relative = 8,
    /// Relocation of a call instruction.  The existing imm field
    /// contains either an offset of the instruction to jump to (think
    /// local function call) or a special value of "-1".  If -1 the
    /// symbol must be looked up in the symbol table.  The relocation
    /// entry contains the symbol number to call.  In order to support
    /// both local jumps and calling external symbols a 32-bit hash is
    /// computed and stored in the the call instruction's 32-bit imm
    /// field.  The hash is used later to look up the 64-bit address
    /// to jump to.  In the case of a local jump the hash is
    /// calculated using the current program counter and in the case
    /// of a symbol the hash is calculated using the name of the
    /// symbol.
    R_Bpf_64_32 = 10,
}
impl BpfRelocationType {
    fn from_x86_relocation_type(from: u32) -> Option<BpfRelocationType> {
        match from {
            R_X86_64_NONE => Some(BpfRelocationType::R_Bpf_None),
            R_X86_64_64 => Some(BpfRelocationType::R_Bpf_64_64),
            R_X86_64_RELATIVE => Some(BpfRelocationType::R_Bpf_64_Relative),
            R_X86_64_32 => Some(BpfRelocationType::R_Bpf_64_32),
            _ => None,
        }
    }
}

#[derive(Debug, PartialEq)]
pub(crate) enum Section {
    /// Owned section data.
    ///
    /// The first field is virtual address of the section.
    /// The second field is the actual section data.
    Owned(usize, Vec<u8>),
    /// Borrowed section data.
    ///
    /// The first field is virtual address of the section.
    /// The second field can be used to index the input ELF buffer to
    /// retrieve the section data.
    Borrowed(usize, Range<usize>),
}

/// Elf loader/relocator
#[derive(Debug, PartialEq)]
pub struct Executable<C: ContextObject> {
    /// Loaded and executable elf
    elf_bytes: AlignedMemory<{ HOST_ALIGN }>,
    /// Required SBPF capabilities
    sbpf_version: SBPFVersion,
    /// Read-only section
    ro_section: Section,
    /// Text section virtual address
    text_section_vaddr: u64,
    /// Text section range in `elf_bytes`
    text_section_range: Range<usize>,
    /// Address of the entry point
    entry_pc: usize,
    /// Call resolution map (hash, pc, name)
    function_registry: FunctionRegistry<usize>,
    /// Loader built-in program
    loader: Arc<BuiltinProgram<C>>,
    /// Compiled program and argument
    #[cfg(all(feature = "jit", not(target_os = "windows"), target_arch = "x86_64"))]
    compiled_program: Option<JitProgram>,
}

impl<C: ContextObject> Executable<C> {
    /// Get the configuration settings
    pub fn get_config(&self) -> &Config {
        self.loader.get_config()
    }

    /// Get the executable sbpf_version
    pub fn get_sbpf_version(&self) -> SBPFVersion {
        self.sbpf_version
    }

    /// Get the .text section virtual address and bytes
    pub fn get_text_bytes(&self) -> (u64, &[u8]) {
        (
            self.text_section_vaddr,
            &self.elf_bytes.as_slice()[self.text_section_range.clone()],
        )
    }

    /// Get the concatenated read-only sections (including the text section)
    pub fn get_ro_section(&self) -> &[u8] {
        match &self.ro_section {
            Section::Owned(_offset, data) => data.as_slice(),
            Section::Borrowed(_offset, byte_range) => {
                &self.elf_bytes.as_slice()[byte_range.clone()]
            }
        }
    }

    /// Get a memory region that can be used to access the merged readonly section
    pub fn get_ro_region(&self) -> MemoryRegion {
        get_ro_region(&self.ro_section, self.elf_bytes.as_slice())
    }

    /// Get the entry point offset into the text section
    pub fn get_entrypoint_instruction_offset(&self) -> usize {
        self.entry_pc
    }

    /// Get the text section offset in the ELF file
    #[cfg(feature = "debugger")]
    pub fn get_text_section_offset(&self) -> u64 {
        self.text_section_range.start as u64
    }

    /// Get the loader built-in program
    pub fn get_loader(&self) -> &Arc<BuiltinProgram<C>> {
        &self.loader
    }

    /// Get the JIT compiled program
    #[cfg(all(feature = "jit", not(target_os = "windows"), target_arch = "x86_64"))]
    pub fn get_compiled_program(&self) -> Option<&JitProgram> {
        self.compiled_program.as_ref()
    }

    /// Verify the executable
    pub fn verify<V: Verifier>(&self) -> Result<(), EbpfError> {
        <V as Verifier>::verify(
            self.get_text_bytes().1,
            self.get_config(),
            self.get_sbpf_version(),
            self.get_function_registry(),
            self.loader.get_function_registry(self.get_sbpf_version()),
        )?;
        Ok(())
    }

    /// JIT compile the executable
    #[cfg(all(feature = "jit", not(target_os = "windows"), target_arch = "x86_64"))]
    pub fn jit_compile(&mut self) -> Result<(), crate::error::EbpfError> {
        let jit = JitCompiler::<C>::new(self)?;
        self.compiled_program = Some(jit.compile()?);
        Ok(())
    }

    /// Get the function registry
    pub fn get_function_registry(&self) -> &FunctionRegistry<usize> {
        &self.function_registry
    }

    /// Create from raw text section bytes (list of instructions)
    pub fn new_from_text_bytes(
        text_bytes: &[u8],
        loader: Arc<BuiltinProgram<C>>,
        sbpf_version: SBPFVersion,
        mut function_registry: FunctionRegistry<usize>,
    ) -> Result<Self, ElfError> {
        let elf_bytes = AlignedMemory::from_slice(text_bytes);
        let entry_pc = if let Some((_name, pc)) = function_registry.lookup_by_name(b"entrypoint") {
            pc
        } else {
            function_registry.register_function_hashed_legacy(
                &loader,
                !sbpf_version.static_syscalls(),
                *b"entrypoint",
                0,
            )?;
            0
        };
        Ok(Self {
            elf_bytes,
            sbpf_version,
            ro_section: Section::Borrowed(ebpf::MM_RODATA_START as usize, 0..text_bytes.len()),
            text_section_vaddr: if sbpf_version.enable_lower_bytecode_vaddr() {
                ebpf::MM_BYTECODE_START
            } else {
                ebpf::MM_RODATA_START
            },
            text_section_range: 0..text_bytes.len(),
            entry_pc,
            function_registry,
            loader,
            #[cfg(all(feature = "jit", not(target_os = "windows"), target_arch = "x86_64"))]
            compiled_program: None,
        })
    }

    /// Fully loads an ELF
    pub fn load(bytes: &[u8], loader: Arc<BuiltinProgram<C>>) -> Result<Self, ElfError> {
        const E_FLAGS_OFFSET: usize = 48;
        let e_flags = LittleEndian::read_u32(
            bytes
                .get(E_FLAGS_OFFSET..E_FLAGS_OFFSET.saturating_add(std::mem::size_of::<u32>()))
                .ok_or(ElfParserError::OutOfBounds)?,
        );
        let config = loader.get_config();
        let sbpf_version = if config.enabled_sbpf_versions.end() == &SBPFVersion::V0 {
            if e_flags == EF_SBPF_V2 {
                SBPFVersion::Reserved
            } else {
                SBPFVersion::V0
            }
        } else {
            match e_flags {
                0 => SBPFVersion::V0,
                1 => SBPFVersion::V1,
                2 => SBPFVersion::V2,
                3 => SBPFVersion::V3,
                _ => SBPFVersion::Reserved,
            }
        };
        if !config.enabled_sbpf_versions.contains(&sbpf_version) {
            return Err(ElfError::UnsupportedSBPFVersion);
        }

        let mut executable = if sbpf_version.enable_stricter_elf_headers() {
            Self::load_with_strict_parser(bytes, loader)?
        } else {
            Self::load_with_lenient_parser(bytes, loader)?
        };
        executable.sbpf_version = sbpf_version;
        Ok(executable)
    }

    /// Loads an ELF without relocation
    fn load_with_strict_parser(
        bytes: &[u8],
        loader: Arc<BuiltinProgram<C>>,
    ) -> Result<Self, ElfParserError> {
        use crate::elf_parser::{
            consts::{
                ELFMAG, EV_CURRENT, PF_R, PF_W, PF_X, PT_GNU_STACK, PT_LOAD, PT_NULL, SHN_UNDEF,
                STT_FUNC,
            },
            types::{Elf64Ehdr, Elf64Shdr, Elf64Sym},
        };

        let aligned_memory = AlignedMemory::<{ HOST_ALIGN }>::from_slice(bytes);
        let elf_bytes = aligned_memory.as_slice();

        let (file_header_range, file_header) = Elf64::parse_file_header(elf_bytes)?;
        let program_header_table_range = mem::size_of::<Elf64Ehdr>()
            ..mem::size_of::<Elf64Phdr>()
                .saturating_mul(file_header.e_phnum as usize)
                .saturating_add(mem::size_of::<Elf64Ehdr>());
        if file_header.e_ident.ei_mag != ELFMAG
            || file_header.e_ident.ei_class != ELFCLASS64
            || file_header.e_ident.ei_data != ELFDATA2LSB
            || file_header.e_ident.ei_version != EV_CURRENT as u8
            || file_header.e_ident.ei_osabi != ELFOSABI_NONE
            || file_header.e_ident.ei_abiversion != 0x00
            || file_header.e_ident.ei_pad != [0x00; 7]
            || file_header.e_type != ET_DYN
            || file_header.e_machine != EM_SBPF
            || file_header.e_version != EV_CURRENT
            // file_header.e_entry
            || file_header.e_phoff != mem::size_of::<Elf64Ehdr>() as u64
            // file_header.e_shoff
            // file_header.e_flags
            || file_header.e_ehsize != mem::size_of::<Elf64Ehdr>() as u16
            || file_header.e_phentsize != mem::size_of::<Elf64Phdr>() as u16
            || file_header.e_phnum < EXPECTED_PROGRAM_HEADERS.len() as u16
            || program_header_table_range.end >= elf_bytes.len()
            || file_header.e_shentsize != mem::size_of::<Elf64Shdr>() as u16
            // file_header.e_shnum
            || file_header.e_shstrndx >= file_header.e_shnum
        {
            return Err(ElfParserError::InvalidFileHeader);
        }

        const EXPECTED_PROGRAM_HEADERS: [(u32, u32, u64); 5] = [
            (PT_LOAD, PF_X, ebpf::MM_BYTECODE_START), // byte code
            (PT_LOAD, PF_R, ebpf::MM_RODATA_START),   // read only data
            (PT_GNU_STACK, PF_R | PF_W, ebpf::MM_STACK_START), // stack
            (PT_LOAD, PF_R | PF_W, ebpf::MM_HEAP_START), // heap
            (PT_NULL, 0, 0xFFFFFFFF00000000),         // dynamic symbol table
        ];
        let program_header_table =
            Elf64::slice_from_bytes::<Elf64Phdr>(elf_bytes, program_header_table_range.clone())?;
        for (program_header, (p_type, p_flags, p_vaddr)) in program_header_table
            .iter()
            .zip(EXPECTED_PROGRAM_HEADERS.iter())
        {
            let p_filesz = if (*p_flags & PF_W) != 0 {
                0
            } else {
                program_header.p_memsz
            };
            if program_header.p_type != *p_type
                || program_header.p_flags != *p_flags
                || program_header.p_offset < program_header_table_range.end as u64
                || program_header.p_offset >= elf_bytes.len() as u64
                || program_header.p_offset.checked_rem(ebpf::INSN_SIZE as u64) != Some(0)
                || program_header.p_vaddr != *p_vaddr
                || program_header.p_paddr != *p_vaddr
                || program_header.p_filesz != p_filesz
                || program_header.p_filesz
                    > (elf_bytes.len() as u64).saturating_sub(program_header.p_offset)
                || program_header.p_memsz >= ebpf::MM_REGION_SIZE
            {
                return Err(ElfParserError::InvalidProgramHeader);
            }
        }

        let config = loader.get_config();
        let symbol_names_section_header = if config.enable_symbol_and_section_labels {
            let (_section_header_table_range, section_header_table) =
                Elf64::parse_section_header_table(
                    elf_bytes,
                    file_header_range.clone(),
                    file_header,
                    program_header_table_range.clone(),
                )?;
            let section_names_section_header = (file_header.e_shstrndx != SHN_UNDEF)
                .then(|| {
                    section_header_table
                        .get(file_header.e_shstrndx as usize)
                        .ok_or(ElfParserError::OutOfBounds)
                })
                .transpose()?
                .ok_or(ElfParserError::NoSectionNameStringTable)?;
            let mut symbol_names_section_header = None;
            for section_header in section_header_table.iter() {
                let section_name = Elf64::get_string_in_section(
                    elf_bytes,
                    section_names_section_header,
                    section_header.sh_name,
                    64,
                )?;
                if section_name == b".dynstr" {
                    symbol_names_section_header = Some(section_header);
                }
            }
            symbol_names_section_header
        } else {
            None
        };
        let bytecode_header = &program_header_table[0];
        let rodata_header = &program_header_table[1];
        let dynamic_symbol_table: &[Elf64Sym] =
            Elf64::slice_from_program_header(elf_bytes, &program_header_table[4])?;
        let mut function_registry = FunctionRegistry::<usize>::default();
        let mut expected_symbol_address = bytecode_header.p_vaddr;
        for symbol in dynamic_symbol_table {
            if symbol.st_info & STT_FUNC == 0 {
                continue;
            }
            if symbol.st_value != expected_symbol_address {
                return Err(ElfParserError::OutOfBounds);
            }
            if symbol.st_size == 0 || symbol.st_size.checked_rem(ebpf::INSN_SIZE as u64) != Some(0)
            {
                return Err(ElfParserError::InvalidSize);
            }
            if symbol.st_size
                > bytecode_header
                    .vm_range()
                    .end
                    .saturating_sub(symbol.st_value)
            {
                return Err(ElfParserError::OutOfBounds);
            }
            let target_pc = symbol
                .st_value
                .saturating_sub(bytecode_header.p_vaddr)
                .checked_div(ebpf::INSN_SIZE as u64)
                .unwrap_or_default() as usize;
            let name = if config.enable_symbol_and_section_labels {
                Elf64::get_string_in_section(
                    elf_bytes,
                    symbol_names_section_header
                        .as_ref()
                        .ok_or(ElfParserError::NoStringTable)?,
                    symbol.st_name as Elf64Word,
                    u8::MAX as usize,
                )?
            } else {
                &[]
            };
            function_registry
                .register_function(target_pc as u32, name, target_pc)
                .unwrap();
            expected_symbol_address = symbol.st_value.saturating_add(symbol.st_size);
        }
        if expected_symbol_address != bytecode_header.vm_range().end {
            return Err(ElfParserError::OutOfBounds);
        }
        if !bytecode_header.vm_range().contains(&file_header.e_entry)
            || file_header.e_entry.checked_rem(ebpf::INSN_SIZE as u64) != Some(0)
        {
            return Err(ElfParserError::InvalidFileHeader);
        }
        let entry_pc = file_header
            .e_entry
            .saturating_sub(bytecode_header.p_vaddr)
            .checked_div(ebpf::INSN_SIZE as u64)
            .unwrap_or_default() as usize;
        if function_registry.lookup_by_key(entry_pc as u32).is_none() {
            return Err(ElfParserError::InvalidFileHeader);
        }

        let text_section_vaddr = bytecode_header.p_vaddr;
        let text_section_range = bytecode_header.file_range().unwrap_or_default();
        let ro_section = Section::Borrowed(
            rodata_header.p_vaddr as usize,
            rodata_header.file_range().unwrap_or_default(),
        );
        Ok(Self {
            elf_bytes: aligned_memory,
            sbpf_version: SBPFVersion::Reserved, // Is set in Self::load()
            ro_section,
            text_section_vaddr,
            text_section_range,
            entry_pc,
            function_registry,
            loader,
            #[cfg(all(feature = "jit", not(target_os = "windows"), target_arch = "x86_64"))]
            compiled_program: None,
        })
    }

    /// Loads an ELF with relocation
    fn load_with_lenient_parser(
        bytes: &[u8],
        loader: Arc<BuiltinProgram<C>>,
    ) -> Result<Self, ElfError> {
        // We always need one memory copy to take ownership and for relocations
        let aligned_memory = AlignedMemory::<{ HOST_ALIGN }>::from_slice(bytes);
        let (mut elf_bytes, unrelocated_elf_bytes) =
            if is_memory_aligned(bytes.as_ptr() as usize, HOST_ALIGN) {
                (aligned_memory, bytes)
            } else {
                // We might need another memory copy to ensure alignment
                (aligned_memory.clone(), aligned_memory.as_slice())
            };
        let elf = Elf64::parse(unrelocated_elf_bytes)?;

        let config = loader.get_config();
        let header = elf.file_header();
        let sbpf_version = if header.e_flags == EF_SBPF_V2 {
            SBPFVersion::Reserved
        } else {
            SBPFVersion::V0
        };

        Self::validate(config, &elf, elf_bytes.as_slice())?;

        // calculate the text section info
        let text_section = get_section(&elf, b".text")?;
        let text_section_vaddr =
            if sbpf_version.enable_elf_vaddr() && text_section.sh_addr >= ebpf::MM_RODATA_START {
                text_section.sh_addr
            } else {
                text_section.sh_addr.saturating_add(ebpf::MM_RODATA_START)
            };
        let vaddr_end = if sbpf_version.reject_rodata_stack_overlap() {
            text_section_vaddr.saturating_add(text_section.sh_size)
        } else {
            text_section_vaddr
        };
        if (config.reject_broken_elfs
            && !sbpf_version.enable_elf_vaddr()
            && text_section.sh_addr != text_section.sh_offset)
            || vaddr_end > ebpf::MM_STACK_START
        {
            return Err(ElfError::ValueOutOfBounds);
        }

        // relocate symbols
        let mut function_registry = FunctionRegistry::default();
        Self::relocate(
            &mut function_registry,
            &loader,
            &elf,
            elf_bytes.as_slice_mut(),
        )?;

        // calculate entrypoint offset into the text section
        let offset = header.e_entry.saturating_sub(text_section.sh_addr);
        if offset.checked_rem(ebpf::INSN_SIZE as u64) != Some(0) {
            return Err(ElfError::InvalidEntrypoint);
        }
        let entry_pc = if let Some(entry_pc) = (offset as usize).checked_div(ebpf::INSN_SIZE) {
            if !sbpf_version.static_syscalls() {
                function_registry.unregister_function(ebpf::hash_symbol_name(b"entrypoint"));
            }
            function_registry.register_function_hashed_legacy(
                &loader,
                !sbpf_version.static_syscalls(),
                *b"entrypoint",
                entry_pc,
            )?;
            entry_pc
        } else {
            return Err(ElfError::InvalidEntrypoint);
        };

        let ro_section = Self::parse_ro_sections(
            config,
            &sbpf_version,
            elf.section_header_table()
                .iter()
                .map(|s| (elf.section_name(s.sh_name).ok(), s)),
            elf_bytes.as_slice(),
        )?;

        Ok(Self {
            elf_bytes,
            sbpf_version,
            ro_section,
            text_section_vaddr,
            text_section_range: text_section.file_range().unwrap_or_default(),
            entry_pc,
            function_registry,
            loader,
            #[cfg(all(feature = "jit", not(target_os = "windows"), target_arch = "x86_64"))]
            compiled_program: None,
        })
    }

    /// Calculate the total memory size of the executable
    #[rustfmt::skip]
    #[allow(clippy::size_of_ref)]
    pub fn mem_size(&self) -> usize {
        let mut total = mem::size_of::<Self>();
        total = total
            // elf bytes
            .saturating_add(self.elf_bytes.mem_size())
            // ro section
            .saturating_add(match &self.ro_section {
                Section::Owned(_, data) => data.capacity(),
                Section::Borrowed(_, _) => 0,
            })
            // bpf functions
            .saturating_add(self.function_registry.mem_size());

        #[cfg(all(feature = "jit", not(target_os = "windows"), target_arch = "x86_64"))]
        {
            // compiled programs
            total = total.saturating_add(self.compiled_program.as_ref().map_or(0, |program| program.mem_size()));
        }

        total
    }

    // Functions exposed for tests

    /// Validates the ELF
    pub fn validate(config: &Config, elf: &Elf64, elf_bytes: &[u8]) -> Result<(), ElfError> {
        let header = elf.file_header();
        if header.e_ident.ei_class != ELFCLASS64 {
            return Err(ElfError::WrongClass);
        }
        if header.e_ident.ei_data != ELFDATA2LSB {
            return Err(ElfError::WrongEndianess);
        }
        if header.e_ident.ei_osabi != ELFOSABI_NONE {
            return Err(ElfError::WrongAbi);
        }
        if header.e_machine != EM_BPF && header.e_machine != EM_SBPF {
            return Err(ElfError::WrongMachine);
        }
        if header.e_type != ET_DYN {
            return Err(ElfError::WrongType);
        }

        let sbpf_version = if header.e_flags == EF_SBPF_V2 {
            SBPFVersion::Reserved
        } else {
            SBPFVersion::V0
        };
        if !config.enabled_sbpf_versions.contains(&sbpf_version) {
            return Err(ElfError::UnsupportedSBPFVersion);
        }

        if sbpf_version.enable_elf_vaddr() {
            if !config.optimize_rodata {
                // When optimize_rodata=false, we allocate a vector and copy all
                // rodata sections into it. In that case we can't allow virtual
                // addresses or we'd potentially have to do huge allocations.
                return Err(ElfError::UnsupportedSBPFVersion);
            }

            // The toolchain currently emits up to 4 program headers. 10 is a
            // future proof nice round number.
            //
            // program_headers() returns an ExactSizeIterator so count doesn't
            // actually iterate again.
            if elf.program_header_table().iter().count() >= 10 {
                return Err(ElfError::InvalidProgramHeader);
            }
        }

        let num_text_sections =
            elf.section_header_table()
                .iter()
                .fold(0, |count: usize, section_header| {
                    if let Ok(this_name) = elf.section_name(section_header.sh_name) {
                        if this_name == b".text" {
                            return count.saturating_add(1);
                        }
                    }
                    count
                });
        if 1 != num_text_sections {
            return Err(ElfError::NotOneTextSection);
        }

        for section_header in elf.section_header_table().iter() {
            if let Ok(name) = elf.section_name(section_header.sh_name) {
                if name.starts_with(b".bss")
                    || (section_header.is_writable()
                        && (name.starts_with(b".data") && !name.starts_with(b".data.rel")))
                {
                    return Err(ElfError::WritableSectionNotSupported(
                        String::from_utf8_lossy(name).to_string(),
                    ));
                }
            }
        }

        for section_header in elf.section_header_table().iter() {
            let start = section_header.sh_offset as usize;
            let end = section_header
                .sh_offset
                .checked_add(section_header.sh_size)
                .ok_or(ElfError::ValueOutOfBounds)? as usize;
            let _ = elf_bytes
                .get(start..end)
                .ok_or(ElfError::ValueOutOfBounds)?;
        }
        let text_section = get_section(elf, b".text")?;
        if !text_section.vm_range().contains(&header.e_entry) {
            return Err(ElfError::EntrypointOutOfBounds);
        }

        Ok(())
    }

    pub(crate) fn parse_ro_sections<
        'a,
        S: IntoIterator<Item = (Option<&'a [u8]>, &'a Elf64Shdr)>,
    >(
        config: &Config,
        sbpf_version: &SBPFVersion,
        sections: S,
        elf_bytes: &[u8],
    ) -> Result<Section, ElfError> {
        // the lowest section address
        let mut lowest_addr = usize::MAX;
        // the highest section address
        let mut highest_addr = 0;
        // the aggregated section length, not including gaps between sections
        let mut ro_fill_length = 0usize;
        let mut invalid_offsets = false;
        // when sbpf_version.enable_elf_vaddr()=true, we allow section_addr != sh_offset
        // if section_addr - sh_offset is constant across all sections. That is,
        // we allow sections to be translated by a fixed virtual offset.
        let mut addr_file_offset = None;

        // keep track of where ro sections are so we can tell whether they're
        // contiguous
        let mut first_ro_section = 0;
        let mut last_ro_section = 0;
        let mut n_ro_sections = 0usize;

        let mut ro_slices = vec![];
        for (i, (name, section_header)) in sections.into_iter().enumerate() {
            match name {
                Some(name)
                    if name == b".text"
                        || name == b".rodata"
                        || name == b".data.rel.ro"
                        || name == b".eh_frame" => {}
                _ => continue,
            }

            if n_ro_sections == 0 {
                first_ro_section = i;
            }
            last_ro_section = i;
            n_ro_sections = n_ro_sections.saturating_add(1);

            let section_addr = section_header.sh_addr;

            // sh_offset handling:
            //
            // If sbpf_version.enable_elf_vaddr()=true, we allow section_addr >
            // sh_offset, if section_addr - sh_offset is constant across all
            // sections. That is, we allow the linker to align rodata to a
            // positive base address (MM_RODATA_START) as long as the mapping
            // to sh_offset(s) stays linear.
            //
            // If sbpf_version.enable_elf_vaddr()=false, section_addr must match
            // sh_offset for backwards compatibility
            if !invalid_offsets {
                if sbpf_version.enable_elf_vaddr() {
                    // This is enforced in validate()
                    debug_assert!(config.optimize_rodata);
                    if section_addr < section_header.sh_offset {
                        invalid_offsets = true;
                    } else {
                        let offset = section_addr.saturating_sub(section_header.sh_offset);
                        if *addr_file_offset.get_or_insert(offset) != offset {
                            // The sections are not all translated by the same
                            // constant. We won't be able to borrow, but unless
                            // config.reject_broken_elf=true, we're still going
                            // to accept this file for backwards compatibility.
                            invalid_offsets = true;
                        }
                    }
                } else if section_addr != section_header.sh_offset {
                    invalid_offsets = true;
                }
            }

            let mut vaddr_end =
                if sbpf_version.enable_elf_vaddr() && section_addr >= ebpf::MM_RODATA_START {
                    section_addr
                } else {
                    section_addr.saturating_add(ebpf::MM_RODATA_START)
                };
            if sbpf_version.reject_rodata_stack_overlap() {
                vaddr_end = vaddr_end.saturating_add(section_header.sh_size);
            }
            if (config.reject_broken_elfs && invalid_offsets) || vaddr_end > ebpf::MM_STACK_START {
                return Err(ElfError::ValueOutOfBounds);
            }

            let section_data = elf_bytes
                .get(section_header.file_range().unwrap_or_default())
                .ok_or(ElfError::ValueOutOfBounds)?;

            let section_addr = section_addr as usize;
            lowest_addr = lowest_addr.min(section_addr);
            highest_addr = highest_addr.max(section_addr.saturating_add(section_data.len()));
            ro_fill_length = ro_fill_length.saturating_add(section_data.len());

            ro_slices.push((section_addr, section_data));
        }

        if config.reject_broken_elfs && lowest_addr.saturating_add(ro_fill_length) > highest_addr {
            return Err(ElfError::ValueOutOfBounds);
        }

        let can_borrow = !invalid_offsets
            && last_ro_section
                .saturating_add(1)
                .saturating_sub(first_ro_section)
                == n_ro_sections;
        if sbpf_version.enable_elf_vaddr() && !can_borrow {
            return Err(ElfError::ValueOutOfBounds);
        }
        let ro_section = if config.optimize_rodata && can_borrow {
            // Read only sections are grouped together with no intermixed non-ro
            // sections. We can borrow.

            // When sbpf_version.enable_elf_vaddr()=true, section addresses and their
            // corresponding buffer offsets can be translated by a constant
            // amount. Subtract the constant to get buffer positions.
            let buf_offset_start =
                lowest_addr.saturating_sub(addr_file_offset.unwrap_or(0) as usize);
            let buf_offset_end =
                highest_addr.saturating_sub(addr_file_offset.unwrap_or(0) as usize);

            let addr_offset = if lowest_addr >= ebpf::MM_RODATA_START as usize {
                // The first field of Section::Borrowed is an offset from
                // ebpf::MM_RODATA_START so if the linker has already put the
                // sections within ebpf::MM_RODATA_START, we need to subtract
                // it now.
                lowest_addr
            } else {
                if sbpf_version.enable_elf_vaddr() {
                    return Err(ElfError::ValueOutOfBounds);
                }
                lowest_addr.saturating_add(ebpf::MM_RODATA_START as usize)
            };

            Section::Borrowed(addr_offset, buf_offset_start..buf_offset_end)
        } else {
            // Read only and other non-ro sections are mixed. Zero the non-ro
            // sections and and copy the ro ones at their intended offsets.

            if config.optimize_rodata {
                // The rodata region starts at MM_RODATA_START + offset,
                // [MM_RODATA_START, MM_RODATA_START + offset) is not
                // mappable. We only need to allocate highest_addr - lowest_addr
                // bytes.
                highest_addr = highest_addr.saturating_sub(lowest_addr);
            } else {
                // For backwards compatibility, the whole [MM_RODATA_START,
                // MM_RODATA_START + highest_addr) range is mappable. We need
                // to allocate the whole address range.
                lowest_addr = 0;
            };

            let buf_len = highest_addr;
            if buf_len > elf_bytes.len() {
                return Err(ElfError::ValueOutOfBounds);
            }

            let mut ro_section = vec![0; buf_len];
            for (section_addr, slice) in ro_slices.iter() {
                let buf_offset_start = section_addr.saturating_sub(lowest_addr);
                ro_section[buf_offset_start..buf_offset_start.saturating_add(slice.len())]
                    .copy_from_slice(slice);
            }

            let addr_offset = if lowest_addr >= ebpf::MM_RODATA_START as usize {
                lowest_addr
            } else {
                lowest_addr.saturating_add(ebpf::MM_RODATA_START as usize)
            };
            Section::Owned(addr_offset, ro_section)
        };

        Ok(ro_section)
    }

    /// Relocates the ELF in-place
    fn relocate(
        function_registry: &mut FunctionRegistry<usize>,
        loader: &BuiltinProgram<C>,
        elf: &Elf64,
        elf_bytes: &mut [u8],
    ) -> Result<(), ElfError> {
        let mut syscall_cache = BTreeMap::new();
        let text_section = get_section(elf, b".text")?;
        let sbpf_version = if elf.file_header().e_flags == EF_SBPF_V2 {
            SBPFVersion::Reserved
        } else {
            SBPFVersion::V0
        };

        // Fixup all program counter relative call instructions
        let config = loader.get_config();
        let text_bytes = elf_bytes
            .get_mut(text_section.file_range().unwrap_or_default())
            .ok_or(ElfError::ValueOutOfBounds)?;
        let instruction_count = text_bytes
            .len()
            .checked_div(ebpf::INSN_SIZE)
            .ok_or(ElfError::ValueOutOfBounds)?;
        for i in 0..instruction_count {
            let insn = ebpf::get_insn(text_bytes, i);
            if insn.opc == ebpf::CALL_IMM && insn.imm != -1 {
                let target_pc = (i as isize)
                    .saturating_add(1)
                    .saturating_add(insn.imm as isize);
                if target_pc < 0 || target_pc >= instruction_count as isize {
                    return Err(ElfError::RelativeJumpOutOfBounds(i));
                }
                let name = if config.enable_symbol_and_section_labels {
                    format!("function_{target_pc}")
                } else {
                    String::default()
                };
                let key = function_registry.register_function_hashed_legacy(
                    loader,
                    !sbpf_version.static_syscalls(),
                    name.as_bytes(),
                    target_pc as usize,
                )?;
                if !sbpf_version.static_syscalls() {
                    let offset = i.saturating_mul(ebpf::INSN_SIZE).saturating_add(4);
                    let checked_slice = text_bytes
                        .get_mut(offset..offset.saturating_add(4))
                        .ok_or(ElfError::ValueOutOfBounds)?;
                    LittleEndian::write_u32(checked_slice, key);
                }
            }
        }

        let mut program_header: Option<&Elf64Phdr> = None;

        // Fixup all the relocations in the relocation section if exists
        for relocation in elf.dynamic_relocations_table().unwrap_or_default().iter() {
            let mut r_offset = relocation.r_offset as usize;

            // When sbpf_version.enable_elf_vaddr()=true, we allow section.sh_addr !=
            // section.sh_offset so we need to bring r_offset to the correct
            // byte offset.
            if sbpf_version.enable_elf_vaddr() {
                match program_header {
                    Some(header) if header.vm_range().contains(&(r_offset as u64)) => {}
                    _ => {
                        program_header = elf
                            .program_header_table()
                            .iter()
                            .find(|header| header.vm_range().contains(&(r_offset as u64)))
                    }
                }
                let header = program_header.as_ref().ok_or(ElfError::ValueOutOfBounds)?;
                r_offset = r_offset
                    .saturating_sub(header.p_vaddr as usize)
                    .saturating_add(header.p_offset as usize);
            }

            match BpfRelocationType::from_x86_relocation_type(relocation.r_type()) {
                Some(BpfRelocationType::R_Bpf_64_64) => {
                    // Offset of the immediate field
                    let imm_offset = if text_section
                        .file_range()
                        .unwrap_or_default()
                        .contains(&r_offset)
                        || sbpf_version == SBPFVersion::V0
                    {
                        r_offset.saturating_add(BYTE_OFFSET_IMMEDIATE)
                    } else {
                        r_offset
                    };

                    // Read the instruction's immediate field which contains virtual
                    // address to convert to physical
                    let checked_slice = elf_bytes
                        .get(imm_offset..imm_offset.saturating_add(BYTE_LENGTH_IMMEDIATE))
                        .ok_or(ElfError::ValueOutOfBounds)?;
                    let refd_addr = LittleEndian::read_u32(checked_slice) as u64;

                    let symbol = elf
                        .dynamic_symbol_table()
                        .and_then(|table| table.get(relocation.r_sym() as usize).cloned())
                        .ok_or_else(|| ElfError::UnknownSymbol(relocation.r_sym() as usize))?;

                    // The relocated address is relative to the address of the
                    // symbol at index `r_sym`
                    let mut addr = symbol.st_value.saturating_add(refd_addr);

                    // The "physical address" from the VM's perspective is rooted
                    // at `MM_RODATA_START`. If the linker hasn't already put
                    // the symbol within `MM_RODATA_START`, we need to do so
                    // now.
                    if addr < ebpf::MM_RODATA_START {
                        addr = ebpf::MM_RODATA_START.saturating_add(addr);
                    }

                    if text_section
                        .file_range()
                        .unwrap_or_default()
                        .contains(&r_offset)
                        || sbpf_version == SBPFVersion::V0
                    {
                        let imm_low_offset = imm_offset;
                        let imm_high_offset = imm_low_offset.saturating_add(INSN_SIZE);

                        // Write the low side of the relocate address
                        let imm_slice = elf_bytes
                            .get_mut(
                                imm_low_offset
                                    ..imm_low_offset.saturating_add(BYTE_LENGTH_IMMEDIATE),
                            )
                            .ok_or(ElfError::ValueOutOfBounds)?;
                        LittleEndian::write_u32(imm_slice, (addr & 0xFFFFFFFF) as u32);

                        // Write the high side of the relocate address
                        let imm_slice = elf_bytes
                            .get_mut(
                                imm_high_offset
                                    ..imm_high_offset.saturating_add(BYTE_LENGTH_IMMEDIATE),
                            )
                            .ok_or(ElfError::ValueOutOfBounds)?;
                        LittleEndian::write_u32(
                            imm_slice,
                            addr.checked_shr(32).unwrap_or_default() as u32,
                        );
                    } else {
                        let imm_slice = elf_bytes
                            .get_mut(imm_offset..imm_offset.saturating_add(8))
                            .ok_or(ElfError::ValueOutOfBounds)?;
                        LittleEndian::write_u64(imm_slice, addr);
                    }
                }
                Some(BpfRelocationType::R_Bpf_64_Relative) => {
                    // Relocation between different sections, where the target
                    // memory is not associated to a symbol (eg some compiler
                    // generated rodata that doesn't have an explicit symbol).

                    // Offset of the immediate field
                    let imm_offset = r_offset.saturating_add(BYTE_OFFSET_IMMEDIATE);

                    if text_section
                        .file_range()
                        .unwrap_or_default()
                        .contains(&r_offset)
                    {
                        // We're relocating a lddw instruction, which spans two
                        // instruction slots. The address to be relocated is
                        // split in two halves in the two imms of the
                        // instruction slots.
                        let imm_low_offset = imm_offset;
                        let imm_high_offset = r_offset
                            .saturating_add(INSN_SIZE)
                            .saturating_add(BYTE_OFFSET_IMMEDIATE);

                        // Read the low side of the address
                        let imm_slice = elf_bytes
                            .get(
                                imm_low_offset
                                    ..imm_low_offset.saturating_add(BYTE_LENGTH_IMMEDIATE),
                            )
                            .ok_or(ElfError::ValueOutOfBounds)?;
                        let va_low = LittleEndian::read_u32(imm_slice) as u64;

                        // Read the high side of the address
                        let imm_slice = elf_bytes
                            .get(
                                imm_high_offset
                                    ..imm_high_offset.saturating_add(BYTE_LENGTH_IMMEDIATE),
                            )
                            .ok_or(ElfError::ValueOutOfBounds)?;
                        let va_high = LittleEndian::read_u32(imm_slice) as u64;

                        // Put the address back together
                        let mut refd_addr = va_high.checked_shl(32).unwrap_or_default() | va_low;

                        if refd_addr == 0 {
                            return Err(ElfError::InvalidVirtualAddress(refd_addr));
                        }

                        if refd_addr < ebpf::MM_RODATA_START {
                            // The linker hasn't already placed rodata within
                            // MM_RODATA_START, so we do so now
                            refd_addr = ebpf::MM_RODATA_START.saturating_add(refd_addr);
                        }

                        // Write back the low half
                        let imm_slice = elf_bytes
                            .get_mut(
                                imm_low_offset
                                    ..imm_low_offset.saturating_add(BYTE_LENGTH_IMMEDIATE),
                            )
                            .ok_or(ElfError::ValueOutOfBounds)?;
                        LittleEndian::write_u32(imm_slice, (refd_addr & 0xFFFFFFFF) as u32);

                        // Write back the high half
                        let imm_slice = elf_bytes
                            .get_mut(
                                imm_high_offset
                                    ..imm_high_offset.saturating_add(BYTE_LENGTH_IMMEDIATE),
                            )
                            .ok_or(ElfError::ValueOutOfBounds)?;
                        LittleEndian::write_u32(
                            imm_slice,
                            refd_addr.checked_shr(32).unwrap_or_default() as u32,
                        );
                    } else {
                        let refd_addr = if sbpf_version != SBPFVersion::V0 {
                            // We're relocating an address inside a data section (eg .rodata). The
                            // address is encoded as a simple u64.

                            let addr_slice = elf_bytes
                                .get(r_offset..r_offset.saturating_add(mem::size_of::<u64>()))
                                .ok_or(ElfError::ValueOutOfBounds)?;
                            let mut refd_addr = LittleEndian::read_u64(addr_slice);
                            if refd_addr < ebpf::MM_RODATA_START {
                                // Not within MM_RODATA_START, do it now
                                refd_addr = ebpf::MM_RODATA_START.saturating_add(refd_addr);
                            }
                            refd_addr
                        } else {
                            // There used to be a bug in toolchains before
                            // https://github.com/solana-labs/llvm-project/pull/35 where for 64 bit
                            // relocations we were encoding only the low 32 bits, shifted 32 bits to
                            // the left. Our relocation code used to be compatible with that, so we
                            // need to keep supporting this case for backwards compatibility.
                            let addr_slice = elf_bytes
                                .get(imm_offset..imm_offset.saturating_add(BYTE_LENGTH_IMMEDIATE))
                                .ok_or(ElfError::ValueOutOfBounds)?;
                            let refd_addr = LittleEndian::read_u32(addr_slice) as u64;
                            ebpf::MM_RODATA_START.saturating_add(refd_addr)
                        };

                        let addr_slice = elf_bytes
                            .get_mut(r_offset..r_offset.saturating_add(mem::size_of::<u64>()))
                            .ok_or(ElfError::ValueOutOfBounds)?;
                        LittleEndian::write_u64(addr_slice, refd_addr);
                    }
                }
                Some(BpfRelocationType::R_Bpf_64_32) => {
                    // The .text section has an unresolved call to symbol instruction
                    // Hash the symbol name and stick it into the call instruction's imm
                    // field.  Later that hash will be used to look up the function location.

                    // Offset of the immediate field
                    let imm_offset = r_offset.saturating_add(BYTE_OFFSET_IMMEDIATE);

                    let symbol = elf
                        .dynamic_symbol_table()
                        .and_then(|table| table.get(relocation.r_sym() as usize).cloned())
                        .ok_or_else(|| ElfError::UnknownSymbol(relocation.r_sym() as usize))?;

                    let name = elf
                        .dynamic_symbol_name(symbol.st_name as Elf64Word)
                        .map_err(|_| ElfError::UnknownSymbol(symbol.st_name as usize))?;

                    // If the symbol is defined, this is a bpf-to-bpf call
                    let key = if symbol.is_function() && symbol.st_value != 0 {
                        if !text_section.vm_range().contains(&symbol.st_value) {
                            return Err(ElfError::ValueOutOfBounds);
                        }
                        let target_pc = (symbol.st_value.saturating_sub(text_section.sh_addr)
                            as usize)
                            .checked_div(ebpf::INSN_SIZE)
                            .unwrap_or_default();
                        function_registry.register_function_hashed_legacy(
                            loader,
                            !sbpf_version.static_syscalls(),
                            name,
                            target_pc,
                        )?
                    } else {
                        // Else it's a syscall
                        let hash = *syscall_cache
                            .entry(symbol.st_name)
                            .or_insert_with(|| ebpf::hash_symbol_name(name));
                        if config.reject_broken_elfs
                            && loader
                                .get_function_registry(SBPFVersion::V0)
                                .lookup_by_key(hash)
                                .is_none()
                        {
                            return Err(ElfError::UnresolvedSymbol(
                                String::from_utf8_lossy(name).to_string(),
                                r_offset.checked_div(ebpf::INSN_SIZE).unwrap_or(0),
                                r_offset,
                            ));
                        }
                        hash
                    };

                    let checked_slice = elf_bytes
                        .get_mut(imm_offset..imm_offset.saturating_add(BYTE_LENGTH_IMMEDIATE))
                        .ok_or(ElfError::ValueOutOfBounds)?;
                    LittleEndian::write_u32(checked_slice, key);
                }
                _ => return Err(ElfError::UnknownRelocation(relocation.r_type())),
            }
        }

        if config.enable_symbol_and_section_labels {
            // Register all known function names from the symbol table
            for symbol in elf.symbol_table().ok().flatten().unwrap_or_default().iter() {
                if symbol.st_info & 0xEF != 0x02 {
                    continue;
                }
                if !text_section.vm_range().contains(&symbol.st_value) {
                    return Err(ElfError::ValueOutOfBounds);
                }
                let target_pc = (symbol.st_value.saturating_sub(text_section.sh_addr) as usize)
                    .checked_div(ebpf::INSN_SIZE)
                    .unwrap_or_default();
                let name = elf
                    .symbol_name(symbol.st_name as Elf64Word)
                    .map_err(|_| ElfError::UnknownSymbol(symbol.st_name as usize))?;
                function_registry.register_function_hashed_legacy(
                    loader,
                    !sbpf_version.static_syscalls(),
                    name,
                    target_pc,
                )?;
            }
        }

        Ok(())
    }

    #[allow(dead_code)]
    fn dump_data(name: &str, prog: &[u8]) {
        let mut eight_bytes: Vec<u8> = Vec::new();
        println!("{name}");
        for i in prog.iter() {
            if eight_bytes.len() >= 7 {
                println!("{eight_bytes:02X?}");
                eight_bytes.clear();
            } else {
                eight_bytes.push(*i);
            }
        }
    }
}

pub(crate) fn get_ro_region(ro_section: &Section, elf: &[u8]) -> MemoryRegion {
    let (offset, ro_data) = match ro_section {
        Section::Owned(offset, data) => (*offset, data.as_slice()),
        Section::Borrowed(offset, byte_range) => (*offset, &elf[byte_range.clone()]),
    };

    // If offset > 0, the region will start at MM_RODATA_START + the offset of
    // the first read only byte. [MM_RODATA_START, MM_RODATA_START + offset)
    // will be unmappable, see MemoryRegion::vm_to_host.
    MemoryRegion::new_readonly(ro_data, offset as u64)
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::{
        elf_parser::{
            // FIXME consts::{ELFCLASS32, ELFDATA2MSB, ET_REL},
            consts::{ELFCLASS32, ELFDATA2MSB, ET_REL},
            types::{Elf64Ehdr, Elf64Shdr, Elf64Sym},
            SECTION_NAME_LENGTH_MAXIMUM,
        },
        error::ProgramResult,
        fuzz::fuzz,
        program::BuiltinFunction,
        syscalls,
        vm::TestContextObject,
    };
    use rand::{distributions::Uniform, Rng};
    use std::{fs::File, io::Read};
    use test_utils::assert_error;
    type ElfExecutable = Executable<TestContextObject>;

    fn loader() -> Arc<BuiltinProgram<TestContextObject>> {
        let mut function_registry =
            FunctionRegistry::<BuiltinFunction<TestContextObject>>::default();
        function_registry
            .register_function_hashed(*b"log", syscalls::SyscallString::vm)
            .unwrap();
        function_registry
            .register_function_hashed(*b"log_64", syscalls::SyscallU64::vm)
            .unwrap();
        Arc::new(BuiltinProgram::new_loader(
            Config::default(),
            function_registry,
        ))
    }

    #[test]
    fn test_strict_header() {
        let elf_bytes =
            std::fs::read("tests/elfs/strict_header.so").expect("failed to read elf file");
        let loader = loader();

        // Check that the unmodified file can be parsed
        {
            let loader = Arc::new(BuiltinProgram::new_loader(
                Config {
                    enable_symbol_and_section_labels: true,
                    ..Config::default()
                },
                FunctionRegistry::<BuiltinFunction<TestContextObject>>::default(),
            ));
            let executable = ElfExecutable::load(&elf_bytes, loader.clone()).unwrap();
            let (name, _pc) = executable.get_function_registry().lookup_by_key(4).unwrap();
            assert_eq!(name, b"entrypoint");
        }

        // Check that using a reserved SBPF version fails
        {
            let mut elf_bytes = elf_bytes.clone();
            elf_bytes[0x0030] = 0xFF;
            let err = ElfExecutable::load(&elf_bytes, loader.clone()).unwrap_err();
            assert_eq!(err, ElfError::UnsupportedSBPFVersion);
        }

        // Check that an empty file fails
        let err = ElfExecutable::load_with_strict_parser(&[], loader.clone()).unwrap_err();
        assert_eq!(err, ElfParserError::OutOfBounds);

        // Break the file header one byte at a time
        let expected_results = std::iter::repeat(&Err(ElfParserError::InvalidFileHeader))
            .take(40)
            .chain(std::iter::repeat(&Ok(())).take(12))
            .chain(std::iter::repeat(&Err(ElfParserError::InvalidFileHeader)).take(4))
            .chain(std::iter::repeat(&Err(ElfParserError::InvalidProgramHeader)).take(1))
            .chain(std::iter::repeat(&Err(ElfParserError::InvalidFileHeader)).take(3))
            .chain(std::iter::repeat(&Ok(())).take(2))
            .chain(std::iter::repeat(&Err(ElfParserError::InvalidFileHeader)).take(2));
        for (offset, expected) in (0..std::mem::size_of::<Elf64Ehdr>()).zip(expected_results) {
            let mut elf_bytes = elf_bytes.clone();
            elf_bytes[offset] = 0xAF;
            let result =
                ElfExecutable::load_with_strict_parser(&elf_bytes, loader.clone()).map(|_| ());
            assert_eq!(&result, expected);
        }

        // Break the program header table one byte at a time
        let expected_results_readonly =
            std::iter::repeat(&Err(ElfParserError::InvalidProgramHeader))
                .take(48)
                .chain(std::iter::repeat(&Ok(())).take(8))
                .collect::<Vec<_>>();
        let expected_results_writable =
            std::iter::repeat(&Err(ElfParserError::InvalidProgramHeader))
                .take(40)
                .chain(std::iter::repeat(&Ok(())).take(4))
                .chain(std::iter::repeat(&Err(ElfParserError::InvalidProgramHeader)).take(4))
                .chain(std::iter::repeat(&Ok(())).take(8))
                .collect::<Vec<_>>();
        let expected_results = vec![
            expected_results_readonly.iter(),
            expected_results_readonly.iter(),
            expected_results_writable.iter(),
            expected_results_writable.iter(),
            expected_results_readonly.iter(),
        ];
        for (header_index, expected_results) in expected_results.into_iter().enumerate() {
            for (offset, expected) in (std::mem::size_of::<Elf64Ehdr>()
                + std::mem::size_of::<Elf64Phdr>() * header_index
                ..std::mem::size_of::<Elf64Ehdr>()
                    + std::mem::size_of::<Elf64Phdr>() * (header_index + 1))
                .zip(expected_results)
            {
                let mut elf_bytes = elf_bytes.clone();
                elf_bytes[offset] = 0xAF;
                let result =
                    ElfExecutable::load_with_strict_parser(&elf_bytes, loader.clone()).map(|_| ());
                assert_eq!(&&result, expected);
            }
        }

        // Break the dynamic symbol table one byte at a time
        for index in 1..3 {
            let expected_results = std::iter::repeat(&Ok(()))
                .take(8)
                .chain(std::iter::repeat(&Err(ElfParserError::OutOfBounds)).take(8))
                .chain(std::iter::repeat(&Err(ElfParserError::InvalidSize)).take(1))
                .chain(std::iter::repeat(&Err(ElfParserError::OutOfBounds)).take(7));
            for (offset, expected) in (0x3000 + std::mem::size_of::<Elf64Sym>() * index
                ..0x3000 + std::mem::size_of::<Elf64Sym>() * (index + 1))
                .zip(expected_results)
            {
                let mut elf_bytes = elf_bytes.clone();
                elf_bytes[offset] = 0xAF;
                let result =
                    ElfExecutable::load_with_strict_parser(&elf_bytes, loader.clone()).map(|_| ());
                assert_eq!(&result, expected);
            }
        }

        // Check that an empty function symbol fails
        {
            let mut elf_bytes = elf_bytes.clone();
            elf_bytes[0x3040] = 0x00;
            let err =
                ElfExecutable::load_with_strict_parser(&elf_bytes, loader.clone()).unwrap_err();
            assert_eq!(err, ElfParserError::InvalidSize);
        }

        // Check that bytecode not covered by function symbols fails
        {
            let mut elf_bytes = elf_bytes.clone();
            elf_bytes[0x3040] = 0x08;
            let err =
                ElfExecutable::load_with_strict_parser(&elf_bytes, loader.clone()).unwrap_err();
            assert_eq!(err, ElfParserError::OutOfBounds);
        }

        // Check that an entrypoint not covered by function symbols fails
        {
            let mut elf_bytes = elf_bytes.clone();
            elf_bytes[0x0018] = 0x10;
            let err =
                ElfExecutable::load_with_strict_parser(&elf_bytes, loader.clone()).unwrap_err();
            assert_eq!(err, ElfParserError::InvalidFileHeader);
        }
    }

    #[test]
    fn test_validate() {
        let elf_bytes = std::fs::read("tests/elfs/relative_call_sbpfv0.so").unwrap();
        let elf = Elf64::parse(&elf_bytes).unwrap();
        let mut header = elf.file_header().clone();

        let config = Config::default();

        let write_header = |header: Elf64Ehdr| unsafe {
            let mut bytes = elf_bytes.clone();
            std::ptr::write(bytes.as_mut_ptr().cast::<Elf64Ehdr>(), header);
            bytes
        };

        ElfExecutable::validate(&config, &elf, &elf_bytes).expect("validation failed");

        header.e_ident.ei_class = ELFCLASS32;
        let bytes = write_header(header.clone());
        // the new parser rejects anything other than ELFCLASS64 directly
        Elf64::parse(&bytes).expect_err("allowed bad class");

        header.e_ident.ei_class = ELFCLASS64;
        let bytes = write_header(header.clone());
        ElfExecutable::validate(&config, &Elf64::parse(&bytes).unwrap(), &elf_bytes)
            .expect("validation failed");

        header.e_ident.ei_data = ELFDATA2MSB;
        let bytes = write_header(header.clone());
        // the new parser only supports little endian
        Elf64::parse(&bytes).expect_err("allowed big endian");

        header.e_ident.ei_data = ELFDATA2LSB;
        let bytes = write_header(header.clone());
        ElfExecutable::validate(&config, &Elf64::parse(&bytes).unwrap(), &elf_bytes)
            .expect("validation failed");

        header.e_ident.ei_osabi = 1;
        let bytes = write_header(header.clone());
        ElfExecutable::validate(&config, &Elf64::parse(&bytes).unwrap(), &elf_bytes)
            .expect_err("allowed wrong abi");

        header.e_ident.ei_osabi = ELFOSABI_NONE;
        let bytes = write_header(header.clone());
        ElfExecutable::validate(&config, &Elf64::parse(&bytes).unwrap(), &elf_bytes)
            .expect("validation failed");

        header.e_machine = 42;
        let bytes = write_header(header.clone());
        ElfExecutable::validate(&config, &Elf64::parse(&bytes).unwrap(), &elf_bytes)
            .expect_err("allowed wrong machine");

        header.e_machine = EM_BPF;
        let bytes = write_header(header.clone());
        ElfExecutable::validate(&config, &Elf64::parse(&bytes).unwrap(), &elf_bytes)
            .expect("validation failed");

        header.e_type = ET_REL;
        let bytes = write_header(header);
        ElfExecutable::validate(&config, &Elf64::parse(&bytes).unwrap(), &elf_bytes)
            .expect_err("allowed wrong type");
    }

    #[test]
    fn test_load() {
        let mut file = File::open("tests/elfs/relative_call_sbpfv0.so").expect("file open failed");
        let mut elf_bytes = Vec::new();
        file.read_to_end(&mut elf_bytes)
            .expect("failed to read elf file");
        ElfExecutable::load(&elf_bytes, loader()).expect("validation failed");
    }

    #[test]
    fn test_load_unaligned() {
        let mut elf_bytes =
            std::fs::read("tests/elfs/relative_call_sbpfv0.so").expect("failed to read elf file");
        // The default allocator allocates aligned memory. Move the ELF slice to
        // elf_bytes.as_ptr() + 1 to make it unaligned and test unaligned
        // parsing.
        elf_bytes.insert(0, 0);
        ElfExecutable::load(&elf_bytes[1..], loader()).expect("validation failed");
    }

    #[test]
    fn test_entrypoint() {
        let loader = loader();

        let mut file = File::open("tests/elfs/relative_call_sbpfv0.so").expect("file open failed");
        let mut elf_bytes = Vec::new();
        file.read_to_end(&mut elf_bytes)
            .expect("failed to read elf file");
        let elf = ElfExecutable::load(&elf_bytes, loader.clone()).expect("validation failed");
        let parsed_elf = Elf64::parse(&elf_bytes).unwrap();
        let executable: &Executable<TestContextObject> = &elf;
        assert_eq!(4, executable.get_entrypoint_instruction_offset());

        let write_header = |header: Elf64Ehdr| unsafe {
            let mut bytes = elf_bytes.clone();
            std::ptr::write(bytes.as_mut_ptr().cast::<Elf64Ehdr>(), header);
            bytes
        };

        let mut header = parsed_elf.file_header().clone();
        let initial_e_entry = header.e_entry;

        header.e_entry += 8;
        let elf_bytes = write_header(header.clone());
        let elf = ElfExecutable::load(&elf_bytes, loader.clone()).expect("validation failed");
        let executable: &Executable<TestContextObject> = &elf;
        assert_eq!(5, executable.get_entrypoint_instruction_offset());

        header.e_entry = 1;
        let elf_bytes = write_header(header.clone());
        assert!(matches!(
            ElfExecutable::load(&elf_bytes, loader.clone()),
            Err(ElfError::EntrypointOutOfBounds)
        ));

        header.e_entry = u64::MAX;
        let elf_bytes = write_header(header.clone());
        assert!(matches!(
            ElfExecutable::load(&elf_bytes, loader.clone()),
            Err(ElfError::EntrypointOutOfBounds)
        ));

        header.e_entry = initial_e_entry + ebpf::INSN_SIZE as u64 + 1;
        let elf_bytes = write_header(header.clone());
        assert!(matches!(
            ElfExecutable::load(&elf_bytes, loader.clone()),
            Err(ElfError::InvalidEntrypoint)
        ));

        header.e_entry = initial_e_entry;
        let elf_bytes = write_header(header);
        let elf = ElfExecutable::load(&elf_bytes, loader).expect("validation failed");
        let executable: &Executable<TestContextObject> = &elf;
        assert_eq!(4, executable.get_entrypoint_instruction_offset());
    }

    #[test]
    #[ignore]
    fn test_fuzz_load() {
        let loader = loader();

        // Random bytes, will mostly fail due to lack of ELF header so just do a few
        let mut rng = rand::thread_rng();
        let range = Uniform::new(0, 255);
        println!("random bytes");
        for _ in 0..1_000 {
            let elf_bytes: Vec<u8> = (0..100).map(|_| rng.sample(range)).collect();
            let _ = ElfExecutable::load(&elf_bytes, loader.clone());
        }

        // Take a real elf and mangle it

        let mut file = File::open("tests/elfs/noop.so").expect("file open failed");
        let mut elf_bytes = Vec::new();
        file.read_to_end(&mut elf_bytes)
            .expect("failed to read elf file");
        let parsed_elf = Elf64::parse(&elf_bytes).unwrap();

        // focus on elf header, small typically 64 bytes
        println!("mangle elf header");
        fuzz(
            &elf_bytes,
            1_000_000,
            100,
            0..parsed_elf.file_header().e_ehsize as usize,
            0..255,
            |bytes: &mut [u8]| {
                let _ = ElfExecutable::load(bytes, loader.clone());
            },
        );

        // focus on section headers
        println!("mangle section headers");
        fuzz(
            &elf_bytes,
            1_000_000,
            100,
            parsed_elf.file_header().e_shoff as usize..elf_bytes.len(),
            0..255,
            |bytes: &mut [u8]| {
                let _ = ElfExecutable::load(bytes, loader.clone());
            },
        );

        // mangle whole elf randomly
        println!("mangle whole elf");
        fuzz(
            &elf_bytes,
            1_000_000,
            100,
            0..elf_bytes.len(),
            0..255,
            |bytes: &mut [u8]| {
                let _ = ElfExecutable::load(bytes, loader.clone());
            },
        );
    }

    fn new_section(sh_addr: u64, sh_size: u64) -> Elf64Shdr {
        Elf64Shdr {
            sh_addr,
            sh_offset: sh_addr
                .checked_sub(ebpf::MM_RODATA_START)
                .unwrap_or(sh_addr),
            sh_size,
            sh_name: 0,
            sh_type: 0,
            sh_flags: 0,
            sh_link: 0,
            sh_info: 0,
            sh_addralign: 0,
            sh_entsize: 0,
        }
    }

    #[test]
    fn test_owned_ro_sections_not_contiguous() {
        let config = Config::default();
        let elf_bytes = [0u8; 512];

        // there's a non-rodata section between two rodata sections
        let s1 = new_section(10, 10);
        let s2 = new_section(20, 10);
        let s3 = new_section(30, 10);

        let sections: [(Option<&[u8]>, &Elf64Shdr); 3] = [
            (Some(b".text"), &s1),
            (Some(b".dynamic"), &s2),
            (Some(b".rodata"), &s3),
        ];
        assert!(matches!(
            ElfExecutable::parse_ro_sections(
                &config,
                &SBPFVersion::V0,
                sections,
                &elf_bytes,
            ),
            Ok(Section::Owned(offset, data)) if offset == ebpf::MM_RODATA_START as usize + 10 && data.len() == 30
        ));
    }

    #[test]
    fn test_owned_ro_sections_with_sh_offset() {
        let config = Config {
            reject_broken_elfs: false,
            ..Config::default()
        };
        let elf_bytes = [0u8; 512];

        // s2 is at a custom sh_offset. We need to merge into an owned buffer so
        // s2 can be moved to the right address offset.
        let s1 = new_section(10, 10);
        let mut s2 = new_section(20, 10);
        s2.sh_offset = 30;

        let sections: [(Option<&[u8]>, &Elf64Shdr); 2] =
            [(Some(b".text"), &s1), (Some(b".rodata"), &s2)];
        assert!(matches!(
            ElfExecutable::parse_ro_sections(
                &config,
                &SBPFVersion::V0,
                sections,
                &elf_bytes,
            ),
            Ok(Section::Owned(offset, data)) if offset == ebpf::MM_RODATA_START as usize + 10 && data.len() == 20
        ));
    }

    #[test]
    fn test_sh_offset_not_same_as_vaddr() {
        let config = Config {
            reject_broken_elfs: true,
            enabled_sbpf_versions: SBPFVersion::V0..=SBPFVersion::V0,
            ..Config::default()
        };
        let elf_bytes = [0u8; 512];

        let mut s1 = new_section(10, 10);

        {
            let sections: [(Option<&[u8]>, &Elf64Shdr); 1] = [(Some(b".text"), &s1)];
            assert!(ElfExecutable::parse_ro_sections(
                &config,
                &SBPFVersion::V0,
                sections,
                &elf_bytes
            )
            .is_ok());
        }

        s1.sh_offset = 0;
        let sections: [(Option<&[u8]>, &Elf64Shdr); 1] = [(Some(b".text"), &s1)];
        assert_eq!(
            ElfExecutable::parse_ro_sections(&config, &SBPFVersion::V0, sections, &elf_bytes),
            Err(ElfError::ValueOutOfBounds)
        );
    }

    #[test]
    fn test_invalid_sh_offset_larger_than_vaddr() {
        let config = Config {
            reject_broken_elfs: true,
            ..Config::default()
        };
        let elf_bytes = [0u8; 512];

        let s1 = new_section(10, 10);
        // sh_offset > sh_addr is invalid
        let mut s2 = new_section(20, 10);
        s2.sh_offset = 30;

        let sections: [(Option<&[u8]>, &Elf64Shdr); 2] =
            [(Some(b".text"), &s1), (Some(b".rodata"), &s2)];
        assert_eq!(
            ElfExecutable::parse_ro_sections(&config, &SBPFVersion::V2, sections, &elf_bytes,),
            Err(ElfError::ValueOutOfBounds)
        );
    }

    #[test]
    fn test_reject_non_constant_sh_offset() {
        let config = Config {
            reject_broken_elfs: true,
            ..Config::default()
        };
        let elf_bytes = [0u8; 512];

        let mut s1 = new_section(ebpf::MM_RODATA_START + 10, 10);
        let mut s2 = new_section(ebpf::MM_RODATA_START + 20, 10);
        // The sections don't have a constant offset. This is rejected since it
        // makes it impossible to efficiently map virtual addresses to byte
        // offsets
        s1.sh_offset = 100;
        s2.sh_offset = 120;

        let sections: [(Option<&[u8]>, &Elf64Shdr); 2] =
            [(Some(b".text"), &s1), (Some(b".rodata"), &s2)];
        assert_eq!(
            ElfExecutable::parse_ro_sections(&config, &SBPFVersion::V3, sections, &elf_bytes),
            Err(ElfError::ValueOutOfBounds)
        );
    }

    #[test]
    fn test_borrowed_ro_sections_with_constant_sh_offset() {
        let config = Config {
            reject_broken_elfs: true,
            ..Config::default()
        };
        let elf_bytes = [0u8; 512];

        let mut s1 = new_section(ebpf::MM_RODATA_START + 10, 10);
        let mut s2 = new_section(ebpf::MM_RODATA_START + 20, 10);
        // the sections have a constant offset (100)
        s1.sh_offset = 100;
        s2.sh_offset = 110;

        let sections: [(Option<&[u8]>, &Elf64Shdr); 2] =
            [(Some(b".text"), &s1), (Some(b".rodata"), &s2)];
        assert_eq!(
            ElfExecutable::parse_ro_sections(&config, &SBPFVersion::V3, sections, &elf_bytes),
            Ok(Section::Borrowed(
                ebpf::MM_RODATA_START as usize + 10,
                100..120
            ))
        );
    }

    #[test]
    fn test_owned_ro_region_no_initial_gap() {
        let config = Config::default();
        let elf_bytes = [0u8; 512];

        // need an owned buffer so we can zero the address space taken by s2
        let s1 = new_section(0, 10);
        let s2 = new_section(10, 10);
        let s3 = new_section(20, 10);

        let sections: [(Option<&[u8]>, &Elf64Shdr); 3] = [
            (Some(b".text"), &s1),
            (Some(b".dynamic"), &s2),
            (Some(b".rodata"), &s3),
        ];
        let ro_section =
            ElfExecutable::parse_ro_sections(&config, &SBPFVersion::V0, sections, &elf_bytes)
                .unwrap();
        let ro_region = get_ro_region(&ro_section, &elf_bytes);
        let owned_section = match &ro_section {
            Section::Owned(_offset, data) => data.as_slice(),
            _ => panic!(),
        };

        // [0..s3.sh_addr + s3.sh_size] is the valid ro memory area
        assert!(matches!(
            ro_region.vm_to_host(ebpf::MM_RODATA_START, s3.sh_addr + s3.sh_size),
            ProgramResult::Ok(ptr) if ptr == owned_section.as_ptr() as u64,
        ));

        // one byte past the ro section is not mappable
        assert_error!(
            ro_region.vm_to_host(ebpf::MM_RODATA_START + s3.sh_addr + s3.sh_size, 1),
            "InvalidVirtualAddress({})",
            ebpf::MM_RODATA_START + s3.sh_addr + s3.sh_size
        );
    }

    #[test]
    fn test_owned_ro_region_initial_gap_mappable() {
        let config = Config {
            optimize_rodata: false,
            ..Config::default()
        };
        let elf_bytes = [0u8; 512];

        // the first section starts at a non-zero offset
        let s1 = new_section(10, 10);
        let s2 = new_section(20, 10);
        let s3 = new_section(30, 10);

        let sections: [(Option<&[u8]>, &Elf64Shdr); 3] = [
            (Some(b".text"), &s1),
            (Some(b".dynamic"), &s2),
            (Some(b".rodata"), &s3),
        ];
        // V2 requires optimize_rodata=true
        let ro_section =
            ElfExecutable::parse_ro_sections(&config, &SBPFVersion::V0, sections, &elf_bytes)
                .unwrap();
        let ro_region = get_ro_region(&ro_section, &elf_bytes);
        let owned_section = match &ro_section {
            Section::Owned(_offset, data) => data.as_slice(),
            _ => panic!(),
        };

        // [s1.sh_addr..s3.sh_addr + s3.sh_size] is where the readonly data is.
        // But for backwards compatibility (config.optimize_rodata=false)
        // [0..s1.sh_addr] is mappable too (and zeroed).
        assert!(matches!(
            ro_region.vm_to_host(ebpf::MM_RODATA_START, s3.sh_addr + s3.sh_size),
            ProgramResult::Ok(ptr) if ptr == owned_section.as_ptr() as u64,
        ));

        // one byte past the ro section is not mappable
        assert_error!(
            ro_region.vm_to_host(ebpf::MM_RODATA_START + s3.sh_addr + s3.sh_size, 1),
            "InvalidVirtualAddress({})",
            ebpf::MM_RODATA_START + s3.sh_addr + s3.sh_size
        );
    }

    #[test]
    fn test_owned_ro_region_initial_gap_map_error() {
        let config = Config::default();
        let elf_bytes = [0u8; 512];

        // the first section starts at a non-zero offset
        let s1 = new_section(10, 10);
        let s2 = new_section(20, 10);
        let s3 = new_section(30, 10);

        let sections: [(Option<&[u8]>, &Elf64Shdr); 3] = [
            (Some(b".text"), &s1),
            (Some(b".dynamic"), &s2),
            (Some(b".rodata"), &s3),
        ];
        let ro_section =
            ElfExecutable::parse_ro_sections(&config, &SBPFVersion::V0, sections, &elf_bytes)
                .unwrap();
        let owned_section = match &ro_section {
            Section::Owned(_offset, data) => data.as_slice(),
            _ => panic!(),
        };
        let ro_region = get_ro_region(&ro_section, &elf_bytes);

        // s1 starts at sh_addr=10 so [MM_RODATA_START..MM_RODATA_START + 10] is not mappable

        // the low bound of the initial gap is not mappable
        assert_error!(
            ro_region.vm_to_host(ebpf::MM_RODATA_START, 1),
            "InvalidVirtualAddress({})",
            ebpf::MM_RODATA_START
        );

        // the hi bound of the initial gap is not mappable
        assert_error!(
            ro_region.vm_to_host(ebpf::MM_RODATA_START + s1.sh_addr - 1, 1),
            "InvalidVirtualAddress({})",
            ebpf::MM_RODATA_START + 9
        );

        // [s1.sh_addr..s3.sh_addr + s3.sh_size] is the valid ro memory area
        assert!(matches!(
            ro_region.vm_to_host(
                ebpf::MM_RODATA_START + s1.sh_addr,
                s3.sh_addr + s3.sh_size - s1.sh_addr
            ),
            ProgramResult::Ok(ptr) if ptr == owned_section.as_ptr() as u64,
        ));

        // one byte past the ro section is not mappable
        assert_error!(
            ro_region.vm_to_host(ebpf::MM_RODATA_START + s3.sh_addr + s3.sh_size, 1),
            "InvalidVirtualAddress({})",
            ebpf::MM_RODATA_START + s3.sh_addr + s3.sh_size
        );
    }

    #[test]
    fn test_borrowed_ro_sections_disabled() {
        let config = Config {
            optimize_rodata: false,
            ..Config::default()
        };
        let elf_bytes = [0u8; 512];

        // s1 and s2 are contiguous, the rodata section can be borrowed from the
        // original elf input but config.borrow_rodata=false
        let s1 = new_section(0, 10);
        let s2 = new_section(10, 10);

        let sections: [(Option<&[u8]>, &Elf64Shdr); 2] =
            [(Some(b".text"), &s1), (Some(b".rodata"), &s2)];
        assert!(matches!(
            ElfExecutable::parse_ro_sections(
                &config,
                &SBPFVersion::V0, // v2 requires optimize_rodata=true
                sections,
                &elf_bytes,
            ),
            Ok(Section::Owned(offset, data)) if offset == ebpf::MM_RODATA_START as usize && data.len() == 20
        ));
    }

    #[test]
    fn test_borrowed_ro_sections() {
        let config = Config::default();
        let elf_bytes = [0u8; 512];
        for (vaddr_base, sbpf_version) in [
            (0, SBPFVersion::V0),
            (ebpf::MM_RODATA_START, SBPFVersion::V3),
        ] {
            let s1 = new_section(vaddr_base, 10);
            let s2 = new_section(vaddr_base + 20, 10);
            let s3 = new_section(vaddr_base + 40, 10);
            let s4 = new_section(vaddr_base + 50, 10);
            let sections: [(Option<&[u8]>, &Elf64Shdr); 4] = [
                (Some(b".dynsym"), &s1),
                (Some(b".text"), &s2),
                (Some(b".rodata"), &s3),
                (Some(b".dynamic"), &s4),
            ];
            assert_eq!(
                ElfExecutable::parse_ro_sections(&config, &sbpf_version, sections, &elf_bytes),
                Ok(Section::Borrowed(
                    ebpf::MM_RODATA_START as usize + 20,
                    20..50
                ))
            );
        }
    }

    #[test]
    fn test_borrowed_ro_region_no_initial_gap() {
        let config = Config::default();
        let elf_bytes = [0u8; 512];
        for (vaddr_base, sbpf_version) in [
            (0, SBPFVersion::V0),
            (ebpf::MM_RODATA_START, SBPFVersion::V3),
        ] {
            let s1 = new_section(vaddr_base, 10);
            let s2 = new_section(vaddr_base + 10, 10);
            let s3 = new_section(vaddr_base + 20, 10);
            let sections: [(Option<&[u8]>, &Elf64Shdr); 3] = [
                (Some(b".text"), &s1),
                (Some(b".rodata"), &s2),
                (Some(b".dynamic"), &s3),
            ];
            let ro_section =
                ElfExecutable::parse_ro_sections(&config, &sbpf_version, sections, &elf_bytes)
                    .unwrap();
            let ro_region = get_ro_region(&ro_section, &elf_bytes);

            // s1 starts at sh_offset=0 so [0..s2.sh_offset + s2.sh_size]
            // is the valid ro memory area
            assert!(matches!(
                ro_region.vm_to_host(ebpf::MM_RODATA_START + s1.sh_offset, s2.sh_offset + s2.sh_size),
                ProgramResult::Ok(ptr) if ptr == elf_bytes.as_ptr() as u64,
            ));

            // one byte past the ro section is not mappable
            assert_error!(
                ro_region.vm_to_host(ebpf::MM_RODATA_START + s3.sh_offset, 1),
                "InvalidVirtualAddress({})",
                ebpf::MM_RODATA_START + s3.sh_offset
            );
        }
    }

    #[test]
    fn test_borrowed_ro_region_initial_gap() {
        let config = Config::default();
        let elf_bytes = [0u8; 512];
        for (vaddr_base, sbpf_version) in [
            (0, SBPFVersion::V0),
            (ebpf::MM_RODATA_START, SBPFVersion::V3),
        ] {
            let s1 = new_section(vaddr_base, 10);
            let s2 = new_section(vaddr_base + 10, 10);
            let s3 = new_section(vaddr_base + 20, 10);
            let sections: [(Option<&[u8]>, &Elf64Shdr); 3] = [
                (Some(b".dynamic"), &s1),
                (Some(b".text"), &s2),
                (Some(b".rodata"), &s3),
            ];
            let ro_section =
                ElfExecutable::parse_ro_sections(&config, &sbpf_version, sections, &elf_bytes)
                    .unwrap();
            let ro_region = get_ro_region(&ro_section, &elf_bytes);

            // s2 starts at sh_addr=10 so [0..10] is not mappable

            // the low bound of the initial gap is not mappable
            assert_error!(
                ro_region.vm_to_host(ebpf::MM_RODATA_START + s1.sh_offset, 1),
                "InvalidVirtualAddress({})",
                ebpf::MM_RODATA_START + s1.sh_offset
            );

            // the hi bound of the initial gap is not mappable
            assert_error!(
                ro_region.vm_to_host(ebpf::MM_RODATA_START + s2.sh_offset - 1, 1),
                "InvalidVirtualAddress({})",
                ebpf::MM_RODATA_START + s2.sh_offset - 1
            );

            // [s2.sh_offset..s3.sh_offset + s3.sh_size] is the valid ro memory area
            assert!(matches!(
                ro_region.vm_to_host(
                    ebpf::MM_RODATA_START + s2.sh_offset,
                    s3.sh_offset + s3.sh_size - s2.sh_offset
                ),
                ProgramResult::Ok(ptr) if ptr == elf_bytes[s2.sh_offset as usize..].as_ptr() as u64,
            ));

            // one byte past the ro section is not mappable
            assert_error!(
                ro_region.vm_to_host(ebpf::MM_RODATA_START + s3.sh_offset + s3.sh_size, 1),
                "InvalidVirtualAddress({})",
                ebpf::MM_RODATA_START + s3.sh_offset + s3.sh_size
            );
        }
    }

    #[test]
    fn test_reject_rodata_stack_overlap() {
        let config = Config {
            enabled_sbpf_versions: SBPFVersion::V0..=SBPFVersion::V3,
            ..Config::default()
        };
        let elf_bytes = [0u8; 512];

        // no overlap
        let mut s1 = new_section(ebpf::MM_STACK_START - 10, 10);
        s1.sh_offset = 0;
        let sections: [(Option<&[u8]>, &Elf64Shdr); 1] = [(Some(b".text"), &s1)];
        assert!(
            ElfExecutable::parse_ro_sections(&config, &SBPFVersion::V3, sections, &elf_bytes)
                .is_ok()
        );

        // no overlap
        let mut s1 = new_section(ebpf::MM_STACK_START, 0);
        s1.sh_offset = 0;
        let sections: [(Option<&[u8]>, &Elf64Shdr); 1] = [(Some(b".text"), &s1)];
        assert!(
            ElfExecutable::parse_ro_sections(&config, &SBPFVersion::V3, sections, &elf_bytes)
                .is_ok()
        );

        // overlap
        let mut s1 = new_section(ebpf::MM_STACK_START, 1);
        s1.sh_offset = 0;
        let sections: [(Option<&[u8]>, &Elf64Shdr); 1] = [(Some(b".text"), &s1)];
        assert_eq!(
            ElfExecutable::parse_ro_sections(&config, &SBPFVersion::V3, sections, &elf_bytes),
            Err(ElfError::ValueOutOfBounds)
        );

        // valid start but start + size overlap
        let mut s1 = new_section(ebpf::MM_STACK_START - 10, 11);
        s1.sh_offset = 0;
        let sections: [(Option<&[u8]>, &Elf64Shdr); 1] = [(Some(b".text"), &s1)];
        assert_eq!(
            ElfExecutable::parse_ro_sections(&config, &SBPFVersion::V3, sections, &elf_bytes),
            Err(ElfError::ValueOutOfBounds)
        );
    }

    #[test]
    #[should_panic(expected = r#"validation failed: WritableSectionNotSupported(".data")"#)]
    fn test_writable_data_section() {
        let elf_bytes =
            std::fs::read("tests/elfs/data_section_sbpfv0.so").expect("failed to read elf file");
        ElfExecutable::load(&elf_bytes, loader()).expect("validation failed");
    }

    #[test]
    #[should_panic(expected = r#"validation failed: WritableSectionNotSupported(".bss")"#)]
    fn test_bss_section() {
        let elf_bytes =
            std::fs::read("tests/elfs/bss_section_sbpfv0.so").expect("failed to read elf file");
        ElfExecutable::load(&elf_bytes, loader()).expect("validation failed");
    }

    #[test]
    #[should_panic(expected = "validation failed: InvalidProgramHeader")]
    fn test_program_headers_overflow() {
        let elf_bytes = std::fs::read("tests/elfs/program_headers_overflow.so")
            .expect("failed to read elf file");
        ElfExecutable::load(&elf_bytes, loader()).expect("validation failed");
    }

    #[test]
    #[should_panic(expected = "validation failed: RelativeJumpOutOfBounds(8)")]
    fn test_relative_call_oob_backward() {
        let mut elf_bytes =
            std::fs::read("tests/elfs/relative_call_sbpfv0.so").expect("failed to read elf file");
        LittleEndian::write_i32(&mut elf_bytes[0x1044..0x1048], -11i32);
        ElfExecutable::load(&elf_bytes, loader()).expect("validation failed");
    }

    #[test]
    #[should_panic(expected = "validation failed: RelativeJumpOutOfBounds(11)")]
    fn test_relative_call_oob_forward() {
        let mut elf_bytes =
            std::fs::read("tests/elfs/relative_call_sbpfv0.so").expect("failed to read elf file");
        LittleEndian::write_i32(&mut elf_bytes[0x105C..0x1060], 5);
        ElfExecutable::load(&elf_bytes, loader()).expect("validation failed");
    }

    #[test]
    #[should_panic(expected = "validation failed: UnresolvedSymbol(\"log\", 39, 312)")]
    fn test_err_unresolved_syscall_reloc_64_32() {
        let loader = BuiltinProgram::new_loader(
            Config {
                enabled_sbpf_versions: SBPFVersion::V0..=SBPFVersion::V0,
                reject_broken_elfs: true,
                ..Config::default()
            },
            FunctionRegistry::default(),
        );
        let elf_bytes = std::fs::read("tests/elfs/syscall_reloc_64_32_sbpfv0.so")
            .expect("failed to read elf file");
        ElfExecutable::load(&elf_bytes, Arc::new(loader)).expect("validation failed");
    }

    #[test]
    fn test_long_section_name() {
        let elf_bytes = std::fs::read("tests/elfs/long_section_name.so").unwrap();
        assert_error!(
            Elf64::parse(&elf_bytes),
            "StringTooLong({:?}, {})",
            ".bss.__rust_no_alloc_shim_is_unstable"
                .get(0..SECTION_NAME_LENGTH_MAXIMUM)
                .unwrap(),
            SECTION_NAME_LENGTH_MAXIMUM
        );
    }
}
