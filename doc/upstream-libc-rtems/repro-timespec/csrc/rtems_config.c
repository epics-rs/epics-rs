/* Smallest RTEMS 6 configuration that boots and calls the Rust `main`. */
#include <rtems.h>
#include <stdlib.h>

extern int main(int argc, char **argv);

void *POSIX_Init(void *arg)
{
  (void) arg;
  char *argv[] = { "rtems-timespec-repro", NULL };
  main(1, argv);
  exit(0);
  return NULL;
}

#define CONFIGURE_APPLICATION_NEEDS_SIMPLE_CONSOLE_DRIVER
#define CONFIGURE_APPLICATION_NEEDS_CLOCK_DRIVER
#define CONFIGURE_MAXIMUM_FILE_DESCRIPTORS 8
/* std needs POSIX keys for its thread-local machinery; without this the image
   dies with "fatal runtime error: out of TLS keys". UNLIMITED_OBJECTS covers
   POSIX_THREADS / POSIX_KEYS / POSIX_KEY_VALUE_PAIRS and requires one of
   UNIFIED_WORK_AREAS or CONFIGURE_EXECUTIVE_RAM_SIZE. */
#define CONFIGURE_UNLIMITED_OBJECTS
#define CONFIGURE_UNLIMITED_ALLOCATION_SIZE 8
#define CONFIGURE_UNIFIED_WORK_AREAS
#define CONFIGURE_POSIX_INIT_THREAD_TABLE
#define CONFIGURE_INIT_THREAD_STACK_SIZE (64 * 1024)
#define CONFIGURE_INIT
#include <rtems/confdefs.h>
