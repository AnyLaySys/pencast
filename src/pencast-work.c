#include <dlfcn.h>
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>

#include <drm/drm.h>
#include <drm/drm_mode.h>
#include <linux/input.h>
#include <linux/uinput.h>
#include <linux/usb/ch9.h>
#include <linux/usb/functionfs.h>

#define DEFAULT_FPS 60U
#define MAX_FPS 60U
#define USB_WRITE_CHUNK_BYTES (128U * 1024U)
#define FRAME_FLAG_JPEG UINT32_C(0x00000001)
#define JPEG_PIXEL_FORMAT 8
#define JPEG_SUBSAMPLING_444 0
#define JPEG_QUALITY 97
#define JPEG_FLAGS (1024 | 2048)
#define INTERFACE_NAME "Codex Mirror Bulk"
#define INTERFACE_GUID "{D4C39B42-BA47-4E8D-83F8-DA4A3B6B8F35}"
#define PROP_NAME "DeviceInterfaceGUIDs"
#define DRM_FORMAT_ARGB8888 UINT32_C(0x34325241)
#define INPUT_MAGIC "CMINPUT1"

#define LE16(value) ((__le16)(value))
#define LE32(value) ((__le32)(value))
#define FFS_MS_OS_DESC_VERSION LE16(1)

enum {
    PROP_NAME_CHARS = sizeof(PROP_NAME),
    GUID_CHARS = sizeof(INTERFACE_GUID),
    GUID_MULTI_SZ_CHARS = GUID_CHARS + 1,
};

struct __attribute__((packed)) ms_compat_descriptor {
    struct usb_os_desc_header header;
    struct usb_ext_compat_desc feature;
};

struct __attribute__((packed)) ms_property_feature {
    __le32 dwSize;
    __le32 dwPropertyDataType;
    __le16 wPropertyNameLength;
    __le16 name[PROP_NAME_CHARS];
    __le32 dwPropertyDataLength;
    __le16 value[GUID_MULTI_SZ_CHARS];
};

struct __attribute__((packed)) ms_property_descriptor {
    struct usb_os_desc_header header;
    struct ms_property_feature feature;
};

struct __attribute__((packed)) descriptor_blob {
    struct usb_functionfs_descs_head_v2 header;
    __le32 fs_count;
    __le32 hs_count;
    __le32 os_count;

    struct usb_interface_descriptor fs_interface;
    struct usb_endpoint_descriptor_no_audio fs_in;
    struct usb_endpoint_descriptor_no_audio fs_out;

    struct usb_interface_descriptor hs_interface;
    struct usb_endpoint_descriptor_no_audio hs_in;
    struct usb_endpoint_descriptor_no_audio hs_out;

    struct ms_compat_descriptor ms_compat;
    struct ms_property_descriptor ms_property;
};

struct __attribute__((packed)) string_blob {
    struct usb_functionfs_strings_head header;
    __le16 language;
    char interface_name[sizeof(INTERFACE_NAME)];
};

struct __attribute__((packed)) start_command {
    char magic[8];
    uint32_t fps;
    uint32_t reserved;
};

struct __attribute__((packed)) stop_command {
    char magic[8];
    uint64_t reserved;
};

struct __attribute__((packed)) input_command {
    char magic[8];
    uint32_t action;
    uint16_t x;
    uint16_t y;
};

enum {
    INPUT_TOUCH_DOWN = 1,
    INPUT_TOUCH_MOVE = 2,
    INPUT_TOUCH_UP = 3,
    INPUT_KEY_DOWN = 4,
    INPUT_KEY_UP = 5,
};

struct __attribute__((packed)) frame_header {
    char magic[8];
    uint32_t width;
    uint32_t height;
    uint32_t pitch;
    uint32_t pixel_format;
    uint64_t sequence;
    uint64_t timestamp_ns;
    uint32_t payload_size;
    uint32_t flags;
};

struct __attribute__((packed)) packet_header {
    char magic[8];
    uint32_t payload_size;
    uint32_t reserved;
};

struct scanout {
    uint32_t fb_id;
    uint32_t width;
    uint32_t height;
    uint32_t pitch;
    uint32_t offset;
    uint32_t pixel_format;
    int dma_buf_fd;
    void *mapping;
    size_t mapping_size;
};

typedef void *tjhandle;
typedef tjhandle (*tj_init_compress)(void);
typedef int (*tj_compress)(tjhandle, const unsigned char *, int, int, int, int,
                           unsigned char **, unsigned long *, int, int, int);
typedef unsigned long (*tj_buffer_size)(int, int, int);
typedef unsigned char *(*tj_alloc)(int);
typedef void (*tj_free)(unsigned char *);
typedef int (*tj_destroy)(tjhandle);

struct jpeg_encoder {
    void *library;
    tjhandle handle;
    unsigned char *buffer;
    unsigned long capacity;
    tj_compress compress;
    tj_buffer_size buffer_size;
    tj_alloc alloc;
    tj_free free;
    tj_destroy destroy;
};

struct touch_input {
    int fd;
    int32_t x_min;
    int32_t x_max;
    int32_t y_min;
    int32_t y_max;
};

struct input_state {
    int endpoint;
    struct touch_input touch;
    int keyboard_fd;
    bool touching;
    _Atomic uint32_t width;
    _Atomic uint32_t height;
    _Atomic bool *streaming;
};

static volatile sig_atomic_t keep_running = 1;
static void on_signal(int signal_number) {
    (void)signal_number;
    keep_running = 0;
}

static uint64_t monotonic_ns(void) {
    struct timespec now;
    clock_gettime(CLOCK_MONOTONIC, &now);
    return (uint64_t)now.tv_sec * UINT64_C(1000000000) + (uint64_t)now.tv_nsec;
}

static int write_all(int fd, const void *buffer, size_t length) {
    const uint8_t *cursor = buffer;

    while (length > 0) {
        size_t request_length = length > USB_WRITE_CHUNK_BYTES ? USB_WRITE_CHUNK_BYTES : length;
        ssize_t written = write(fd, cursor, request_length);
        if (written < 0) {
            if (errno == EINTR) {
                continue;
            }
            return -1;
        }
        if (written == 0) {
            errno = EIO;
            return -1;
        }
        cursor += written;
        length -= (size_t)written;
    }
    return 0;
}

static void init_interface(struct usb_interface_descriptor *descriptor) {
    memset(descriptor, 0, sizeof(*descriptor));
    descriptor->bLength = USB_DT_INTERFACE_SIZE;
    descriptor->bDescriptorType = USB_DT_INTERFACE;
    descriptor->bNumEndpoints = 2;
    descriptor->bInterfaceClass = USB_CLASS_VENDOR_SPEC;
    descriptor->bInterfaceSubClass = 0x42;
    descriptor->bInterfaceProtocol = 0x01;
    descriptor->iInterface = 1;
}

static void init_endpoint(struct usb_endpoint_descriptor_no_audio *descriptor,
                           uint8_t address, uint16_t packet_size) {
    memset(descriptor, 0, sizeof(*descriptor));
    descriptor->bLength = USB_DT_ENDPOINT_SIZE;
    descriptor->bDescriptorType = USB_DT_ENDPOINT;
    descriptor->bEndpointAddress = address;
    descriptor->bmAttributes = USB_ENDPOINT_XFER_BULK;
    descriptor->wMaxPacketSize = LE16(packet_size);
}

static void copy_utf16le(__le16 *destination, const char *source, size_t source_length) {
    for (size_t index = 0; index < source_length; ++index) {
        destination[index] = LE16((unsigned char)source[index]);
    }
}

static void init_descriptors(struct descriptor_blob *descriptors) {
    memset(descriptors, 0, sizeof(*descriptors));
    descriptors->header.magic = LE32(FUNCTIONFS_DESCRIPTORS_MAGIC_V2);
    descriptors->header.length = LE32(sizeof(*descriptors));
    descriptors->header.flags = LE32(FUNCTIONFS_HAS_FS_DESC |
                                     FUNCTIONFS_HAS_HS_DESC |
                                     FUNCTIONFS_HAS_MS_OS_DESC);
    descriptors->fs_count = LE32(3);
    descriptors->hs_count = LE32(3);
    descriptors->os_count = LE32(2);

    init_interface(&descriptors->fs_interface);
    init_endpoint(&descriptors->fs_in, USB_DIR_IN | 1, 64);
    init_endpoint(&descriptors->fs_out, USB_DIR_OUT | 2, 64);
    init_interface(&descriptors->hs_interface);
    init_endpoint(&descriptors->hs_in, USB_DIR_IN | 1, 512);
    init_endpoint(&descriptors->hs_out, USB_DIR_OUT | 2, 512);

    descriptors->ms_compat.header.interface = 0;
    descriptors->ms_compat.header.dwLength =
        LE32(sizeof(struct usb_os_desc_header) + sizeof(struct usb_ext_compat_desc));
    descriptors->ms_compat.header.bcdVersion = FFS_MS_OS_DESC_VERSION;
    descriptors->ms_compat.header.wIndex = LE16(4);
    descriptors->ms_compat.header.bCount = 1;
    descriptors->ms_compat.feature.bFirstInterfaceNumber = 0;
    descriptors->ms_compat.feature.Reserved1 = 1;
    memcpy(descriptors->ms_compat.feature.CompatibleID, "WINUSB", 6);

    descriptors->ms_property.header.interface = 0;
    descriptors->ms_property.header.dwLength =
        LE32(sizeof(struct usb_os_desc_header) + sizeof(struct ms_property_feature));
    descriptors->ms_property.header.bcdVersion = FFS_MS_OS_DESC_VERSION;
    descriptors->ms_property.header.wIndex = LE16(5);
    descriptors->ms_property.header.wCount = LE16(1);
    descriptors->ms_property.feature.dwSize = LE32(sizeof(struct ms_property_feature));
    descriptors->ms_property.feature.dwPropertyDataType = LE32(7);
    descriptors->ms_property.feature.wPropertyNameLength = LE16(PROP_NAME_CHARS * 2U);
    copy_utf16le(descriptors->ms_property.feature.name, PROP_NAME, PROP_NAME_CHARS);
    descriptors->ms_property.feature.dwPropertyDataLength = LE32(GUID_MULTI_SZ_CHARS * 2U);
    copy_utf16le(descriptors->ms_property.feature.value, INTERFACE_GUID, GUID_CHARS);
}

static void init_strings(struct string_blob *strings) {
    memset(strings, 0, sizeof(*strings));
    strings->header.magic = LE32(FUNCTIONFS_STRINGS_MAGIC);
    strings->header.length = LE32(sizeof(*strings));
    strings->header.str_count = LE32(1);
    strings->header.lang_count = LE32(1);
    strings->language = LE16(0x0409);
    memcpy(strings->interface_name, INTERFACE_NAME, sizeof(INTERFACE_NAME));
}

static int open_endpoint(const char *mount_path, const char *endpoint_name, int flags) {
    char path[512];
    int count = snprintf(path, sizeof(path), "%s/%s", mount_path, endpoint_name);
    if (count < 0 || (size_t)count >= sizeof(path)) {
        errno = ENAMETOOLONG;
        return -1;
    }
    return open(path, flags);
}

static int input_write(int fd, uint16_t type, uint16_t code, int32_t value) {
    struct input_event event = {
        .type = type,
        .code = code,
        .value = value,
    };
    return write_all(fd, &event, sizeof(event));
}

static bool input_has(const unsigned long *bits, unsigned int code) {
    return (bits[code / (sizeof(*bits) * CHAR_BIT)] >> (code % (sizeof(*bits) * CHAR_BIT))) & 1U;
}

static int input_open_touch(struct touch_input *touch) {
    unsigned long types[(EV_MAX + sizeof(unsigned long) * CHAR_BIT) /
                        (sizeof(unsigned long) * CHAR_BIT)] = {0};
    unsigned long axes[(ABS_MAX + sizeof(unsigned long) * CHAR_BIT) /
                       (sizeof(unsigned long) * CHAR_BIT)] = {0};
    DIR *directory = opendir("/dev/input");
    if (directory == NULL) {
        return -1;
    }
    for (struct dirent *entry; (entry = readdir(directory)) != NULL;) {
        if (strncmp(entry->d_name, "event", 5) != 0 || entry->d_name[5] == '\0') {
            continue;
        }
        char path[PATH_MAX];
        int length = snprintf(path, sizeof(path), "/dev/input/%s", entry->d_name);
        if (length < 0 || (size_t)length >= sizeof(path)) {
            continue;
        }
        int fd = open(path, O_RDWR | O_CLOEXEC);
        if (fd < 0) {
            continue;
        }
        memset(types, 0, sizeof(types));
        memset(axes, 0, sizeof(axes));
        if (ioctl(fd, EVIOCGBIT(0, sizeof(types)), types) < 0 || !input_has(types, EV_ABS) ||
            ioctl(fd, EVIOCGBIT(EV_ABS, sizeof(axes)), axes) < 0 ||
            !input_has(axes, ABS_MT_POSITION_X) || !input_has(axes, ABS_MT_POSITION_Y)) {
            close(fd);
            continue;
        }
        struct input_absinfo x;
        struct input_absinfo y;
        if (ioctl(fd, EVIOCGABS(ABS_MT_POSITION_X), &x) != 0 ||
            ioctl(fd, EVIOCGABS(ABS_MT_POSITION_Y), &y) != 0 || x.maximum <= x.minimum ||
            y.maximum <= y.minimum) {
            close(fd);
            continue;
        }
        closedir(directory);
        *touch = (struct touch_input){
            .fd = fd,
            .x_min = x.minimum,
            .x_max = x.maximum,
            .y_min = y.minimum,
            .y_max = y.maximum,
        };
        return 0;
    }
    closedir(directory);
    errno = ENODEV;
    return -1;
}

static int input_open_keyboard(void) {
    int fd = open("/dev/uinput", O_WRONLY | O_NONBLOCK | O_CLOEXEC);
    struct uinput_user_dev device = {0};
    if (fd < 0) {
        return -1;
    }
    if (ioctl(fd, UI_SET_EVBIT, EV_KEY) != 0) {
        close(fd);
        return -1;
    }
    for (int key = 1; key < BTN_MISC; ++key) {
        if (ioctl(fd, UI_SET_KEYBIT, key) != 0) {
            close(fd);
            return -1;
        }
    }
    memcpy(device.name, "PenCast Keyboard", sizeof("PenCast Keyboard"));
    device.id.bustype = BUS_VIRTUAL;
    device.id.vendor = 1;
    device.id.product = 1;
    device.id.version = 1;
    if (write_all(fd, &device, sizeof(device)) != 0 || ioctl(fd, UI_DEV_CREATE) != 0) {
        close(fd);
        return -1;
    }
    return fd;
}

static void input_close(int fd) {
    if (fd >= 0) {
        close(fd);
    }
}

static int32_t input_scale(uint16_t value, uint32_t length, int32_t minimum, int32_t maximum) {
    if (length <= 1 || maximum <= minimum) {
        return minimum;
    }
    return minimum +
           (int32_t)((uint64_t)value * (uint64_t)(maximum - minimum) / (uint64_t)(length - 1));
}

static int input_touch(struct input_state *state, uint32_t action, uint16_t x, uint16_t y) {
    struct touch_input *touch = &state->touch;
    int fd = touch->fd;
    if (action == INPUT_TOUCH_DOWN) {
        state->touching = true;
        if (input_write(fd, EV_ABS, ABS_MT_SLOT, 0) != 0 ||
            input_write(fd, EV_ABS, ABS_MT_TRACKING_ID, 1) != 0 ||
            input_write(fd, EV_KEY, BTN_TOUCH, 1) != 0) {
            return -1;
        }
    } else if (action == INPUT_TOUCH_UP) {
        if (!state->touching) {
            return 0;
        }
        state->touching = false;
        if (input_write(fd, EV_ABS, ABS_MT_SLOT, 0) != 0 ||
            input_write(fd, EV_ABS, ABS_MT_TRACKING_ID, -1) != 0 ||
            input_write(fd, EV_KEY, BTN_TOUCH, 0) != 0) {
            return -1;
        }
    } else if (!state->touching) {
        return 0;
    }
    uint32_t width = atomic_load(&state->width);
    uint32_t height = atomic_load(&state->height);
    if (action != INPUT_TOUCH_UP &&
        (width == 0 || height == 0 ||
         input_write(fd, EV_ABS, ABS_MT_POSITION_X,
                     input_scale(x, width, touch->x_min, touch->x_max)) != 0 ||
         input_write(fd, EV_ABS, ABS_MT_POSITION_Y,
                     input_scale(y, height, touch->y_min, touch->y_max)) != 0 ||
         input_write(fd, EV_ABS, ABS_MT_TOUCH_MAJOR, 1) != 0 ||
         input_write(fd, EV_ABS, ABS_MT_PRESSURE, 1) != 0)) {
        return -1;
    }
    return input_write(fd, EV_SYN, SYN_REPORT, 0);
}

static int input_key(int fd, uint16_t key, uint32_t action) {
    if (key == 0 || key >= BTN_MISC) {
        errno = EINVAL;
        return -1;
    }
    if (input_write(fd, EV_KEY, key, action == INPUT_KEY_DOWN) != 0) {
        return -1;
    }
    return input_write(fd, EV_SYN, SYN_REPORT, 0);
}

static void scanout_release(struct scanout *scanout) {
    if (scanout->mapping != MAP_FAILED && scanout->mapping != NULL) {
        munmap(scanout->mapping, scanout->mapping_size);
    }
    if (scanout->dma_buf_fd >= 0) {
        close(scanout->dma_buf_fd);
    }
    memset(scanout, 0, sizeof(*scanout));
    scanout->dma_buf_fd = -1;
    scanout->mapping = MAP_FAILED;
}

static int drm_find_plane(int fd, uint32_t *plane_id) {
    struct drm_set_client_cap capability = {
        .capability = DRM_CLIENT_CAP_UNIVERSAL_PLANES,
        .value = 1,
    };
    if (ioctl(fd, DRM_IOCTL_SET_CLIENT_CAP, &capability) != 0) {
        return -1;
    }
    struct drm_mode_get_plane_res resources = {0};
    if (ioctl(fd, DRM_IOCTL_MODE_GETPLANERESOURCES, &resources) != 0 || resources.count_planes == 0) {
        return -1;
    }
    uint32_t *planes = calloc(resources.count_planes, sizeof(*planes));
    if (planes == NULL) {
        return -1;
    }
    resources.plane_id_ptr = (uintptr_t)planes;
    if (ioctl(fd, DRM_IOCTL_MODE_GETPLANERESOURCES, &resources) != 0) {
        int saved_errno = errno;
        free(planes);
        errno = saved_errno;
        return -1;
    }
    uint64_t largest = 0;
    for (uint32_t index = 0; index < resources.count_planes; ++index) {
        struct drm_mode_get_plane plane = {.plane_id = planes[index]};
        if (ioctl(fd, DRM_IOCTL_MODE_GETPLANE, &plane) != 0 || plane.crtc_id == 0 ||
            plane.fb_id == 0) {
            continue;
        }
        struct drm_mode_fb_cmd2 framebuffer = {.fb_id = plane.fb_id};
        if (ioctl(fd, DRM_IOCTL_MODE_GETFB2, &framebuffer) != 0) {
            continue;
        }
        if (framebuffer.handles[0] != 0) {
            struct drm_gem_close close_request = {.handle = framebuffer.handles[0]};
            ioctl(fd, DRM_IOCTL_GEM_CLOSE, &close_request);
        }
        uint64_t area = (uint64_t)framebuffer.width * (uint64_t)framebuffer.height;
        if (area > largest) {
            largest = area;
            *plane_id = plane.plane_id;
        }
    }
    free(planes);
    if (largest == 0) {
        errno = ENODATA;
        return -1;
    }
    return 0;
}

static int drm_open(uint32_t *plane_id) {
    DIR *directory = opendir("/dev/dri");
    if (directory == NULL) {
        return -1;
    }
    int result = -1;
    for (struct dirent *entry; (entry = readdir(directory)) != NULL;) {
        if (strncmp(entry->d_name, "card", 4) != 0 || entry->d_name[4] == '\0') {
            continue;
        }
        char path[PATH_MAX];
        int length = snprintf(path, sizeof(path), "/dev/dri/%s", entry->d_name);
        if (length < 0 || (size_t)length >= sizeof(path)) {
            continue;
        }
        int fd = open(path, O_RDWR | O_CLOEXEC);
        if (fd < 0) {
            continue;
        }
        if (drm_find_plane(fd, plane_id) == 0) {
            result = fd;
            break;
        }
        close(fd);
    }
    closedir(directory);
    if (result < 0) {
        errno = ENODEV;
    }
    return result;
}

static int scanout_update(struct scanout *scanout, int drm_fd, uint32_t plane_id) {
    struct drm_mode_get_plane plane = {.plane_id = plane_id};
    if (ioctl(drm_fd, DRM_IOCTL_MODE_GETPLANE, &plane) != 0) {
        return -1;
    }
    if (plane.fb_id == 0) {
        errno = ENODATA;
        return -1;
    }
    if (scanout->fb_id == plane.fb_id) {
        return 0;
    }

    scanout_release(scanout);

    struct drm_mode_fb_cmd2 framebuffer = {.fb_id = plane.fb_id};
    if (ioctl(drm_fd, DRM_IOCTL_MODE_GETFB2, &framebuffer) != 0) {
        return -1;
    }
    if (framebuffer.handles[0] == 0 || framebuffer.width == 0 || framebuffer.height == 0 ||
        framebuffer.width > UINT32_MAX / 4U || framebuffer.pitches[0] < framebuffer.width * 4U) {
        errno = ENOTSUP;
        return -1;
    }

    struct drm_prime_handle prime = {
        .handle = framebuffer.handles[0],
        .flags = DRM_CLOEXEC | DRM_RDWR,
        .fd = -1,
    };
    int export_result = ioctl(drm_fd, DRM_IOCTL_PRIME_HANDLE_TO_FD, &prime);
    struct drm_gem_close close_request = {.handle = framebuffer.handles[0]};
    int close_result = ioctl(drm_fd, DRM_IOCTL_GEM_CLOSE, &close_request);
    if (export_result != 0) {
        return -1;
    }
    if (close_result != 0) {
        close(prime.fd);
        return -1;
    }

    size_t mapping_size = (size_t)framebuffer.offsets[0] +
                          (size_t)framebuffer.pitches[0] * (size_t)framebuffer.height;
    void *mapping = mmap(NULL, mapping_size, PROT_READ, MAP_SHARED, prime.fd, 0);
    if (mapping == MAP_FAILED) {
        int saved_errno = errno;
        close(prime.fd);
        errno = saved_errno;
        return -1;
    }

    scanout->fb_id = plane.fb_id;
    scanout->width = framebuffer.width;
    scanout->height = framebuffer.height;
    scanout->pitch = framebuffer.pitches[0];
    scanout->offset = framebuffer.offsets[0];
    scanout->pixel_format = framebuffer.pixel_format;
    scanout->dma_buf_fd = prime.fd;
    scanout->mapping = mapping;
    scanout->mapping_size = mapping_size;
    return 0;
}

static void jpeg_release(struct jpeg_encoder *encoder) {
    if (encoder->buffer != NULL && encoder->free != NULL) {
        encoder->free(encoder->buffer);
    }
    if (encoder->handle != NULL && encoder->destroy != NULL) {
        encoder->destroy(encoder->handle);
    }
    if (encoder->library != NULL) {
        dlclose(encoder->library);
    }
    memset(encoder, 0, sizeof(*encoder));
}

static int jpeg_open(struct jpeg_encoder *encoder) {
    tj_init_compress init;

    encoder->library = dlopen("libturbojpeg.so.0", RTLD_NOW | RTLD_LOCAL);
    if (encoder->library == NULL) {
        errno = ENOENT;
        return -1;
    }
    init = (tj_init_compress)dlsym(encoder->library, "tjInitCompress");
    encoder->compress = (tj_compress)dlsym(encoder->library, "tjCompress2");
    encoder->buffer_size = (tj_buffer_size)dlsym(encoder->library, "tjBufSize");
    encoder->alloc = (tj_alloc)dlsym(encoder->library, "tjAlloc");
    encoder->free = (tj_free)dlsym(encoder->library, "tjFree");
    encoder->destroy = (tj_destroy)dlsym(encoder->library, "tjDestroy");
    if (init == NULL || encoder->compress == NULL || encoder->buffer_size == NULL ||
        encoder->alloc == NULL || encoder->free == NULL || encoder->destroy == NULL) {
        errno = ENOSYS;
        jpeg_release(encoder);
        return -1;
    }
    encoder->handle = init();
    if (encoder->handle == NULL) {
        errno = EIO;
        jpeg_release(encoder);
        return -1;
    }
    return 0;
}

static int jpeg_prepare(struct jpeg_encoder *encoder, uint32_t width, uint32_t height) {
    if (width > INT_MAX || height > INT_MAX) {
        errno = EOVERFLOW;
        return -1;
    }
    unsigned long capacity =
        encoder->buffer_size((int)width, (int)height, JPEG_SUBSAMPLING_444);
    if (capacity == 0 || capacity > INT_MAX) {
        errno = EOVERFLOW;
        return -1;
    }
    if (encoder->buffer != NULL && encoder->capacity >= capacity) {
        return 0;
    }
    unsigned char *buffer = encoder->alloc((int)capacity);
    if (buffer == NULL) {
        errno = ENOMEM;
        return -1;
    }
    if (encoder->buffer != NULL) {
        encoder->free(encoder->buffer);
    }
    encoder->buffer = buffer;
    encoder->capacity = capacity;
    return 0;
}

static int jpeg_encode(struct jpeg_encoder *encoder, const uint8_t *source, uint32_t width,
                       uint32_t height, unsigned long *encoded_size) {
    if (jpeg_prepare(encoder, width, height) != 0) {
        return -1;
    }
    unsigned char *buffer = encoder->buffer;
    unsigned long size = encoder->capacity;
    if (encoder->compress(encoder->handle, source, (int)width, (int)(width * 4U), (int)height,
                          JPEG_PIXEL_FORMAT, &buffer, &size, JPEG_SUBSAMPLING_444,
                          JPEG_QUALITY, JPEG_FLAGS) != 0 ||
        buffer != encoder->buffer || size == 0 || size > UINT32_MAX) {
        errno = EIO;
        return -1;
    }
    *encoded_size = size;
    return 0;
}

static int send_frame(int endpoint, struct scanout *scanout, int drm_fd, uint32_t plane_id,
                      struct input_state *input, uint8_t **scratch, size_t *scratch_size,
                      struct jpeg_encoder *encoder, bool *config_pending,
                      struct frame_header *stream_config) {
    if (scanout_update(scanout, drm_fd, plane_id) != 0) {
        return -1;
    }
    if (scanout->pixel_format != DRM_FORMAT_ARGB8888) {
        errno = ENOTSUP;
        return -1;
    }
    atomic_store(&input->width, scanout->width);
    atomic_store(&input->height, scanout->height);
    size_t row = (size_t)scanout->width * 4U;
    if (scanout->height > SIZE_MAX / row) {
        errno = EOVERFLOW;
        return -1;
    }
    size_t packed_size = row * (size_t)scanout->height;
    if (packed_size > UINT32_MAX) {
        errno = EOVERFLOW;
        return -1;
    }
    const uint8_t *pixels = (const uint8_t *)scanout->mapping + scanout->offset;
    if (*scratch_size < packed_size) {
        uint8_t *replacement = realloc(*scratch, packed_size);
        if (replacement == NULL) {
            errno = ENOMEM;
            return -1;
        }
        *scratch = replacement;
        *scratch_size = packed_size;
    }
    for (uint32_t index = 0; index < scanout->height; ++index) {
        memcpy(*scratch + (size_t)index * row, pixels + (size_t)index * scanout->pitch, row);
    }
    if (*config_pending) {
        *stream_config = (struct frame_header){
            .magic = {'C', 'M', 'C', 'O', 'N', 'F', 'I', 'G'},
            .width = scanout->width,
            .height = scanout->height,
            .pitch = (uint32_t)row,
            .pixel_format = scanout->pixel_format,
            .sequence = 0,
            .timestamp_ns = monotonic_ns(),
            .payload_size = (uint32_t)packed_size,
            .flags = FRAME_FLAG_JPEG,
        };
        if (write_all(endpoint, stream_config, sizeof(*stream_config)) != 0) {
            return -1;
        }
        *config_pending = false;
    } else if (stream_config->width != scanout->width ||
               stream_config->height != scanout->height ||
               stream_config->pitch != row ||
               stream_config->pixel_format != scanout->pixel_format ||
               stream_config->payload_size != packed_size) {
        errno = EPIPE;
        return -1;
    }
    unsigned long length = 0;
    if (jpeg_encode(encoder, *scratch, scanout->width, scanout->height, &length) != 0) {
        return -1;
    }
    struct packet_header packet = {
        .magic = {'C', 'M', 'J', 'P', 'E', 'G', '0', '1'},
        .payload_size = (uint32_t)length,
    };
    if (write_all(endpoint, &packet, sizeof(packet)) != 0 ||
        write_all(endpoint, encoder->buffer, (size_t)length) != 0) {
        return -1;
    }
    return 0;
}

static int handle_events(int ep0, const char *mount_path, int *ep_in, int *ep_out,
                         _Atomic bool *streaming) {
    struct usb_functionfs_event events[8];
    ssize_t read_count = read(ep0, events, sizeof(events));
    if (read_count < 0) {
        return errno == EINTR || errno == EAGAIN ? 0 : -1;
    }
    if (read_count == 0 || (read_count % sizeof(events[0])) != 0) {
        errno = EPROTO;
        return -1;
    }

    for (size_t index = 0; index < (size_t)read_count / sizeof(events[0]); ++index) {
        switch (events[index].type) {
        case FUNCTIONFS_ENABLE:
            if (*ep_in < 0) {
                *ep_in = open_endpoint(mount_path, "ep1", O_WRONLY);
            }
            if (*ep_out < 0) {
                *ep_out = open_endpoint(mount_path, "ep2", O_RDONLY | O_NONBLOCK);
            }
            if (*ep_in < 0 || *ep_out < 0) {
                return -1;
            }
            break;
        case FUNCTIONFS_DISABLE:
        case FUNCTIONFS_UNBIND:
            if (*ep_in >= 0) {
                close(*ep_in);
                *ep_in = -1;
            }
            if (*ep_out >= 0) {
                close(*ep_out);
                *ep_out = -1;
            }
            atomic_store(streaming, false);
            break;
        default:
            break;
        }
    }
    return 0;
}

static int handle_command(const uint8_t *command, struct input_state *state,
                           bool *config_pending, uint32_t *fps, uint64_t *next_frame_ns) {
    if (memcmp(command, "CMSTART1", 8) == 0) {
        const struct start_command *start = (const struct start_command *)command;
        uint32_t requested_fps = start->fps;
        *fps = requested_fps == 0 ? DEFAULT_FPS : requested_fps;
        if (*fps > MAX_FPS) {
            *fps = MAX_FPS;
        }
        atomic_store(state->streaming, true);
        *config_pending = true;
        *next_frame_ns = 0;
    } else if (memcmp(command, "CMSTOP01", 8) == 0) {
        atomic_store(state->streaming, false);
    } else if (memcmp(command, INPUT_MAGIC, 8) == 0) {
        const struct input_command *input = (const struct input_command *)command;
        int result;
        if (input->action >= INPUT_TOUCH_DOWN && input->action <= INPUT_TOUCH_UP) {
            result = input_touch(state, input->action, input->x, input->y);
        } else if (input->action == INPUT_KEY_DOWN || input->action == INPUT_KEY_UP) {
            result = input_key(state->keyboard_fd, input->x, input->action);
        } else {
            errno = EINVAL;
            return -1;
        }
        if (result != 0) {
            return -1;
        }
    }
    return 1;
}

static int handle_control(int endpoint, struct input_state *input, bool *config_pending,
                          uint32_t *fps, uint64_t *next_frame_ns) {
    uint8_t commands[64];
    ssize_t length = read(endpoint, commands, sizeof(commands));
    if (length < 0) {
        return errno == EAGAIN || errno == EINTR ? 0 : -1;
    }
    if (length == 0) {
        return 0;
    }
    if ((size_t)length % sizeof(struct input_command) != 0) {
        errno = EPROTO;
        return -1;
    }
    for (size_t offset = 0; offset < (size_t)length; offset += sizeof(struct input_command)) {
        if (handle_command(commands + offset, input, config_pending, fps, next_frame_ns) < 0) {
            return -1;
        }
    }
    return 1;
}

static void *input_loop(void *argument) {
    struct input_state *state = argument;
    uint8_t commands[64];
    while (keep_running) {
        ssize_t length = read(state->endpoint, commands, sizeof(commands));
        if (length < 0) {
            if (errno == EINTR) {
                continue;
            }
            break;
        }
        if (length == 0 || (size_t)length % sizeof(struct input_command) != 0) {
            break;
        }
        for (size_t offset = 0; offset < (size_t)length; offset += sizeof(struct input_command)) {
            const uint8_t *command = commands + offset;
            if (memcmp(command, "CMSTOP01", 8) == 0) {
                atomic_store(state->streaming, false);
                continue;
            }
            if (memcmp(command, INPUT_MAGIC, 8) != 0) {
                continue;
            }
            const struct input_command *input = (const struct input_command *)command;
            int result;
            if (input->action >= INPUT_TOUCH_DOWN && input->action <= INPUT_TOUCH_UP) {
                result = input_touch(state, input->action, input->x, input->y);
            } else if (input->action == INPUT_KEY_DOWN || input->action == INPUT_KEY_UP) {
                result = input_key(state->keyboard_fd, input->x, input->action);
            } else {
                continue;
            }
            if (result != 0) {
                return NULL;
            }
        }
    }
    return NULL;
}

int main(int argc, char **argv) {
    const char *mount_path = "/dev/usb-ffs/mirror";
    uint32_t fps = DEFAULT_FPS;
    struct descriptor_blob descriptors;
    struct string_blob strings;
    char ep0_path[512];
    int ep0 = -1;
    int ep_in = -1;
    int ep_out = -1;
    int drm_fd = -1;
    uint32_t plane_id = 0;
    pthread_t input_thread;
    struct scanout scanout = {.dma_buf_fd = -1, .mapping = MAP_FAILED};
    _Atomic bool streaming = false;
    struct input_state input = {
        .endpoint = -1,
        .touch = {.fd = -1},
        .keyboard_fd = -1,
        .streaming = &streaming,
    };
    uint8_t *scratch = NULL;
    size_t scratch_size = 0;
    struct jpeg_encoder encoder = {0};
    bool config_pending = true;
    bool input_started = false;
    struct frame_header stream_config = {0};
    uint64_t next_frame_ns = 0;

    for (int index = 1; index < argc; ++index) {
        if (strcmp(argv[index], "--mount") == 0 && index + 1 < argc) {
            mount_path = argv[++index];
        } else if (strcmp(argv[index], "--fps") == 0 && index + 1 < argc) {
            fps = (uint32_t)strtoul(argv[++index], NULL, 0);
            if (fps == 0 || fps > MAX_FPS) {
                fprintf(stderr, "fps must be between 1 and %u\n", MAX_FPS);
                return 2;
            }
        } else {
            fprintf(stderr, "Usage: %s [--mount PATH] [--fps N]\n", argv[0]);
            return 2;
        }
    }

    signal(SIGINT, on_signal);
    signal(SIGTERM, on_signal);
    signal(SIGPIPE, SIG_IGN);

    int path_length = snprintf(ep0_path, sizeof(ep0_path), "%s/ep0", mount_path);
    if (path_length < 0 || (size_t)path_length >= sizeof(ep0_path)) {
        fprintf(stderr, "ep0 path is too long\n");
        return 2;
    }
    ep0 = open(ep0_path, O_RDWR | O_NONBLOCK);
    if (ep0 < 0) {
        perror("open ep0");
        goto cleanup;
    }
    drm_fd = drm_open(&plane_id);
    if (drm_fd < 0) {
        perror("open DRM device");
        goto cleanup;
    }
    if (scanout_update(&scanout, drm_fd, plane_id) != 0) {
        perror("read DRM plane");
        goto cleanup;
    }
    if (jpeg_open(&encoder) != 0) {
        perror("open TurboJPEG");
        goto cleanup;
    }
    if (input_open_touch(&input.touch) != 0) {
        perror("open touch input");
        goto cleanup;
    }
    input.keyboard_fd = input_open_keyboard();
    if (input.keyboard_fd < 0) {
        perror("create virtual keyboard");
        goto cleanup;
    }

    init_descriptors(&descriptors);
    init_strings(&strings);
    if (write_all(ep0, &descriptors, sizeof(descriptors)) != 0 ||
        write_all(ep0, &strings, sizeof(strings)) != 0) {
        perror("register FunctionFS descriptors");
        goto cleanup;
    }
    while (keep_running) {
        if (!atomic_load(&streaming) || ep_in < 0) {
            struct pollfd fds[2] = {
                {.fd = ep0, .events = POLLIN},
                {.fd = ep_out, .events = POLLIN},
            };
            nfds_t count = !input_started && ep_out >= 0 ? 2 : 1;
            int poll_result = poll(fds, count, 1000);
            if (poll_result < 0) {
                if (errno == EINTR) {
                    continue;
                }
                perror("poll FunctionFS events");
                break;
            }
            if (poll_result > 0 && (fds[0].revents & (POLLIN | POLLERR | POLLHUP))) {
                if (handle_events(ep0, mount_path, &ep_in, &ep_out, &streaming) != 0) {
                    perror("FunctionFS event");
                    break;
                }
            }
            if (!input_started && ep_out >= 0 && fds[1].revents & POLLIN) {
                if (handle_control(ep_out, &input, &config_pending, &fps, &next_frame_ns) < 0) {
                    perror("handle control");
                    break;
                }
            }
            if (!input_started && atomic_load(&streaming)) {
                int flags = fcntl(ep_out, F_GETFL);
                if (flags < 0 || fcntl(ep_out, F_SETFL, flags & ~O_NONBLOCK) != 0) {
                    perror("configure control endpoint");
                    break;
                }
                input.endpoint = ep_out;
                int create_result = pthread_create(&input_thread, NULL, input_loop, &input);
                if (create_result != 0) {
                    errno = create_result;
                    perror("start input thread");
                    break;
                }
                input_started = true;
            }
            continue;
        }

        uint64_t now = monotonic_ns();
        if (next_frame_ns != 0 && now < next_frame_ns) {
            uint64_t remaining_ns = next_frame_ns - now;
            struct timespec delay = {
                .tv_sec = (time_t)(remaining_ns / UINT64_C(1000000000)),
                .tv_nsec = (long)(remaining_ns % UINT64_C(1000000000)),
            };
            nanosleep(&delay, NULL);
            continue;
        }
        if (send_frame(ep_in, &scanout, drm_fd, plane_id, &input, &scratch, &scratch_size,
                       &encoder, &config_pending, &stream_config) != 0) {
            perror("send frame");
            atomic_store(&streaming, false);
            continue;
        }
        uint64_t interval_ns = UINT64_C(1000000000) / fps;
        next_frame_ns = next_frame_ns == 0 ? now + interval_ns : next_frame_ns + interval_ns;
    }

cleanup:
    if (input_started) {
        pthread_cancel(input_thread);
        pthread_join(input_thread, NULL);
    }
    if (input.keyboard_fd >= 0) {
        ioctl(input.keyboard_fd, UI_DEV_DESTROY);
    }
    input_close(input.keyboard_fd);
    input_close(input.touch.fd);
    jpeg_release(&encoder);
    free(scratch);
    scanout_release(&scanout);
    if (drm_fd >= 0) {
        close(drm_fd);
    }
    if (ep_out >= 0) {
        close(ep_out);
    }
    if (ep_in >= 0) {
        close(ep_in);
    }
    if (ep0 >= 0) {
        close(ep0);
    }
    return 0;
}
