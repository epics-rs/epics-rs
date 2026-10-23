// RegexpConverter.cc called the way StreamCore calls it, for
// tests/regexp_converter.rs (see run-c.sh).
//
// Each stdin line is mode<TAB>format<TAB>arg[<TAB>n]. The format starts at
// its '%'; a \xNN in it is that byte. Mode p parses it as an output format
// and runs printPseudo on arg; mode s parses it as an input format and runs
// scanString on arg with an n-byte buffer (16 when left out), or
// scanPseudo on arg with cursor n (0 when left out). arg is hex bytes, "."
// for none; scanString reads it from a zeroed 512-byte buffer, so a width
// past its end matches 0x00 bytes. Each stdout line is what parseFormat
// returned, the format text left after it, the info string less the
// compiled pattern's pointer, the call's result and what StreamError
// printed, bytes in hex.
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <string>
#include <vector>
#include "StreamFormatConverter.h"
#include "StreamProtocol.h"
#include "StreamError.h"

static std::string hexdec(const char* h) {
    std::string s; if (h[0] == 0x2e) return s; while (h[0] && h[1]) { unsigned v; sscanf(h, "%2x", &v); s.push_back((char)v); h += 2; } return s;
}
static void hexout(const char* p, size_t n) { for (size_t i = 0; i < n; i++) printf("%02x", (unsigned char)p[i]); }

int main() {
    streamError = 1;
    streamMsgTimeStamped = 0; streamDebugColored = 0;
    extern FILE* StreamDebugFile;
    char line[8192];
    setvbuf(stdout, NULL, _IONBF, 0);
    while (fgets(line, sizeof line, stdin)) {
        line[strcspn(line, "\n")] = 0;
        std::vector<char*> f; char* save; for (char* t = strtok_r(line, "\t", &save); t; t = strtok_r(NULL, "\t", &save)) f.push_back(t);
        if (f.size() < 3) continue;
        bool scan = f[0][0] == 's';
        std::string fs;
        for (const char* p = f[1]; *p; p++) {
            unsigned v;
            if (p[0] == '\\' && p[1] == 'x' && sscanf(p + 2, "%2x", &v) == 1) { fs.push_back((char)v); p += 3; }
            else fs.push_back(*p);
        }
        const char* src = fs.c_str();
        StreamFormat fmt; StreamBuffer info;
        memset(&fmt, 0, sizeof fmt);
        char* eb = NULL; size_t el = 0; StreamDebugFile = open_memstream(&eb, &el);
        int t = StreamFormatConverter::parseFormat(src, scan ? ScanFormat : PrintFormat, fmt, info);
        size_t skip = info.length() >= sizeof(void*) ? sizeof(void*) : 0;
        printf("parse=%d rest=", t); hexout(src, strlen(src));
        printf(" infolen=%zu info=", info.length()); hexout(info(skip), info.length() - skip);
        if (t) {
            fmt.type = (StreamFormatType)t; fmt.info = info(); fmt.infolen = info.length();
            StreamFormatConverter* c = StreamFormatConverter::find(fmt.conv);
            std::string v = hexdec(f[2]);
            if (t == pseudo_format) {
                StreamBuffer b(v.data(), v.size());
                if (scan) {
                    size_t cursor = f.size() > 3 ? strtoul(f[3], NULL, 0) : 0;
                    ssize_t n = c->scanPseudo(fmt, b, cursor);
                    printf(" n=%zd cursor=%zu out=", n, cursor);
                } else {
                    bool ok = c->printPseudo(fmt, b);
                    printf(" ok=%d out=", ok);
                }
                hexout(b(), b.length());
            } else {
                static char in[512]; memset(in, 0, sizeof in); memcpy(in, v.data(), v.size());
                size_t size = f.size() > 3 ? strtoul(f[3], NULL, 0) : 16;
                char buf[256]; memset(buf, 0x5a, sizeof buf);
                size_t sz = size;
                ssize_t n = c->scanString(fmt, in, buf, sz);
                printf(" n=%zd size=%zu buf=", n, sz); hexout(buf, size);
            }
        }
        fclose(StreamDebugFile); StreamDebugFile = NULL;
        printf(" err="); hexout(eb, el); free(eb);
        printf("\n");
    }
}
