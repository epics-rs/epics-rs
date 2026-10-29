program prim_types
option +r;
char p_char;
unsigned char p_uchar;
short p_short;
unsigned short p_ushort;
int p_int;
unsigned int p_uint;
long p_long;
unsigned long p_ulong;
int8_t p_int8t;
uint8_t p_uint8t;
int16_t p_int16t;
uint16_t p_uint16t;
int32_t p_int32t;
uint32_t p_uint32t;
float p_float;
double p_double;
string p_string;
assign p_char to "char";
assign p_uchar to "uchar";
assign p_short to "short";
assign p_ushort to "ushort";
assign p_int to "int";
assign p_uint to "uint";
assign p_long to "long";
assign p_ulong to "ulong";
assign p_int8t to "int8t";
assign p_uint8t to "uint8t";
assign p_int16t to "int16t";
assign p_uint16t to "uint16t";
assign p_int32t to "int32t";
assign p_uint32t to "uint32t";
assign p_float to "float";
assign p_double to "double";
assign p_string to "string";
ss main {
    state idle {
        when () {} state idle
    }
}
