// Test-only converter on conversion byte 0xE9, for the dbior byte oracle.
#include "StreamFormatConverter.h"

class HighConverter : public StreamFormatConverter
{
    int parse(const StreamFormat&, StreamBuffer&, const char*&, bool)
    {
        return signed_format;
    }
};

RegisterConverter (HighConverter, "\xe9");
