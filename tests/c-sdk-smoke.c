#include <errno.h>
#include <libkrun.h>
#include <stdint.h>
#include <stdio.h>

#define CHECK(condition) do { \
    if (!(condition)) { \
        fprintf(stderr, "C SDK check failed at line %d: %s\n", __LINE__, #condition); \
        return 1; \
    } \
} while (0)

int main(void)
{
    int first = krun_create_ctx();
    int second = krun_create_ctx();
    uint32_t first_cid = 0, second_cid = 0;
    const uint32_t pinned_cid = 1U << 24;

    CHECK(first >= 0 && second >= 0 && first != second);
    CHECK(krun_get_guest_cid(first, &first_cid) == 0);
    CHECK(krun_get_guest_cid(second, &second_cid) == 0);
    CHECK(first_cid >= 3 && second_cid >= 3 && first_cid != second_cid);
    CHECK(krun_get_guest_cid(first, NULL) == -EINVAL);
    CHECK(krun_get_guest_cid(UINT32_MAX, &first_cid) == -ENOENT);
    for (uint32_t reserved = 0; reserved < 3; reserved++)
        CHECK(krun_set_guest_cid(first, reserved) == -EINVAL);
    CHECK(krun_set_guest_cid(first, pinned_cid) == 0);
    CHECK(krun_get_guest_cid(first, &first_cid) == 0);
    CHECK(first_cid == pinned_cid);
    CHECK(krun_set_guest_cid(first, UINT32_MAX) == -EINVAL);
    CHECK(krun_get_guest_cid(first, &first_cid) == 0);
    CHECK(first_cid == pinned_cid);
    CHECK(krun_set_guest_cid(second, pinned_cid) == -EEXIST);
    CHECK(krun_get_guest_cid(second, &second_cid) == 0);
    CHECK(second_cid != first_cid);
    CHECK(krun_set_guest_cid(second, UINT32_MAX - 1) == 0);
    CHECK(krun_get_guest_cid(second, &second_cid) == 0);
    CHECK(second_cid == UINT32_MAX - 1);
    CHECK(krun_free_ctx(first) == 0);
    CHECK(krun_get_guest_cid(first, &first_cid) == -ENOENT);
    CHECK(krun_free_ctx(second) == 0);
    puts("PASS: C SDK context lifetime, CID assignment, override and rejection");
    return 0;
}
