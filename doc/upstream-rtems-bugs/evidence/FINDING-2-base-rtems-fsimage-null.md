# EPICS base: `epicsRtemsFSImage = NULL` faults before `main()` on RTEMS

Applies to EPICS base 7.0.10 (`bf11a0c`), `modules/libcom/RTEMS/posix/rtems_init.c`.
Reproduced on RTEMS 6.0.0, BSP `xilinx_zynq_a9_qemu`, arm-rtems6-gcc 13.3.0.
Base source is **unpatched**; the whole application is 12 lines.

## The supported configuration that crashes

`rtems_init.c` documents three states for `epicsRtemsFSImage`.  The middle one
is the "I do not need a filesystem" declaration:

```c
205  const epicsMemFS *epicsRtemsFSImage __attribute__((weak));
206  const epicsMemFS *epicsRtemsFSImage = (void*)&epicsRtemsFSImage;
...
212  epicsRtemsMountLocalFilesystem(char **argv)
213  {
214      if(epicsRtemsFSImage==(void*)&epicsRtemsFSImage)
215          return -1; /* no FS image provided. */
216      else if(epicsRtemsFSImage==NULL)
217          return 0;  /* no FS image provided, but none is needed. */
218      else {
...
224              argv[1] = "/";
225              return 0;
```

Taking that documented path kills the guest before `main()` runs.

## Minimal reproduction

```c
#include <stdio.h>
#include <epicsMemFs.h>

const epicsMemFS *epicsRtemsFSImage = NULL;   /* documented: none is needed */

int main(int argc, char **argv)
{
    printf("FSBUG: main() reached -- bug is NOT present\n");
    return 0;
}
```

Link as a normal `PROD_IOC` for an RTEMS target and boot.  Observed:

```
***** Setting up file system *****
***** Initializing NFS *****
 check for time registered , C++ initialization ...
***** Preparing EPICS application *****

*** FATAL ***
fatal source: 9 (RTEMS_FATAL_SOURCE_EXCEPTION)

R0   = 0x00000000 R8  = 0x00000010
R1   = 0x0000002f R9  = 0x00000000
...
PC  = 0x002dc238
```

`R0 = 0` is the NULL string argument; `R1 = 0x2f` is `'/'`.  Resolving the PC:

```
$ arm-rtems6-addr2line -f -e fsbug.exe 0x002dc238
strchr
newlib/libc/string/strchr.c:100
```

reached from `strrchr` (`0x002dca98` in the same image; newlib's `strrchr`
tail-calls `strchr`).  `main()` is never entered.

## The exact NULL and how it reaches `strrchr`

All line numbers in `modules/libcom/RTEMS/posix/rtems_init.c`:

1. **948** `POSIX_Init` declares the startup argument vector, all NULL:
   ```c
   char *argv[3] = { NULL, NULL, NULL };
   ```
2. **1127** `initialize_remote_filesystem(argv, initialize_local_filesystem(argv));`
3. **238** `initialize_local_filesystem` calls the weak hook and, on `0`,
   reports success: `if (epicsRtemsMountLocalFilesystem(argv)==0) return 1;`
4. **216-217** the hook returns `0` for the `NULL` image **without assigning
   `argv[1]`**.  Every other successful path assigns it (line 224 `argv[1]="/"`,
   line 256, line 315, line 339, line 366, line 411).
5. **293** `initialize_remote_filesystem(argv, hasLocalFilesystem=1)` guards all
   of its own `argv[1] = ...` assignments with `if (!hasLocalFilesystem)`, so it
   correctly leaves it alone.
6. **1164** `set_directory (argv[1]);`  -- `argv[1]` is still `NULL`.
7. **471**, inside `set_directory`:
   ```c
   465  set_directory (const char *commandline)
   466  {
   467      const char *cp;
   ...
   471      cp = strrchr(commandline, '/');
   472      if (cp == NULL) {          /* handles "no slash", not "no string" */
   ```
   `strrchr(NULL, '/')` -> fault.

Line 1165 `epicsEnvSet ("IOC_STARTUP_SCRIPT", argv[1])` and line 1184
`result = main (..., argv)` are both downstream of the fault and would also
need to cope with a NULL `argv[1]`.

The check at 472 shows the author expected a path without a `/`, but not the
absence of a path, which is precisely what "no filesystem is needed" means.

## What a correct fix looks like

The contract is that a successful `initialize_local_filesystem` leaves a usable
startup path in `argv[1]`, and the `NULL`-image branch is the one path that
returns success without honouring it.  Two options; the first is preferable
because it keeps the invariant with the code that establishes it:

**A. Honour the invariant at the branch that breaks it** (`rtems_init.c:216-217`):

```c
    else if(epicsRtemsFSImage==NULL) {
        argv[1] = "/";   /* no image needed; run from the root of the IMFS */
        return 0;        /* no FS image provided, but none is needed. */
    }
```

**B. Make the consumers total**, so no future path can reintroduce it:

```c
    /* 1164 */
    if (argv[1] == NULL)
        argv[1] = "/";
    set_directory (argv[1]);
```

and/or make `set_directory` accept NULL by treating it exactly as the existing
"no slash" case:

```c
    cp = commandline ? strrchr(commandline, '/') : NULL;
```

Doing both A and B is cheap and leaves no configuration that can fault.

Note that with either fix `IOC_STARTUP_SCRIPT` becomes `"/"`, and `main()` is
then reached with `argv[1] == "/"`; an application that declared it needs no
filesystem is by definition not going to read a script from it, so this is
consistent with the documented intent.

## Workaround used by this panel meanwhile

Define the weak hook in the application and assign `argv[1]` there:

```c
int epicsRtemsMountLocalFilesystem(char **argv) { argv[1] = "/"; return 0; }
```

This is an override of a base hook, not a patch to base.
