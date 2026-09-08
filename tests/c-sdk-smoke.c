#include <libkrun.h>

int main(void)
{
    int context = krun_create_ctx();
    if (context < 0)
        return 1;
    return krun_free_ctx(context) != 0;
}
