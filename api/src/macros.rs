// Macros shared within the API crate
// Align instruction discriminators to 8 bytes: [tag, 0,0,0,0,0,0,0]
// Usage: instruction8!(EnumDiscriminator, StructName);
// This implements to_bytes() adding the 8-byte header and try_from_bytes() for the struct.
macro_rules! instruction8 {
    ($discriminator_name:ident, $struct_name:ident) => {
        impl $struct_name {
            pub fn to_bytes(&self) -> Vec<u8> {
                let mut v = vec![$discriminator_name::$struct_name as u8, 0, 0, 0, 0, 0, 0, 0];
                v.extend_from_slice(bytemuck::bytes_of(self));
                v
            }
            pub fn try_from_bytes(
                data: &[u8],
            ) -> Result<&Self, solana_program::program_error::ProgramError> {
                bytemuck::try_from_bytes::<Self>(data).or(Err(
                    solana_program::program_error::ProgramError::InvalidInstructionData,
                ))
            }
        }
    };
}
