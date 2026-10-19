// SPDX-License-Identifier: GPL-3.0-or-later
//
// macOS proof of concept for xrizer: an OpenVR scene application in miniature.
//
// Loads xrizer's bin/osx32/vrclient.dylib as openvr_api would, initialises it, creates a
// Vulkan instance and device through MoltenVK with the extensions xrizer asks for, and
// submits an animated colour to each eye for a number of frames. xrizer turns those
// submissions into an OpenXR session on the active runtime (Monado), so a run exercises
// the whole OpenVR -> xrizer -> OpenXR (Vulkan) -> Monado path without a game.
//
//   vulkan_submit_probe /path/to/xrizer/target/release/bin/osx32/vrclient.dylib [frames]
//
// Success: the simulated (or real) HMD shows red in the left eye and blue in the right,
// both pulsing, and the head pose printed once a second follows the head.

#include <vulkan/vulkan.h>

#include "../../openvr/headers/openvr-2.15.6.h"
#include "../../openvr/headers/ivrclientcore_003.h"

#include <dlfcn.h>

#include <cmath>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <sstream>
#include <string>
#include <vector>

#define CHECK_VK(call)                                                                             \
	do {                                                                                           \
		VkResult result_ = (call);                                                                 \
		if (result_ != VK_SUCCESS) {                                                               \
			std::fprintf(stderr, "%s failed: %d\n", #call, (int)result_);                          \
			std::exit(1);                                                                          \
		}                                                                                          \
	} while (0)

static std::vector<std::string> split(const char *text)
{
	std::vector<std::string> out;
	std::istringstream stream(text);
	for (std::string word; stream >> word;)
		out.push_back(word);
	return out;
}

static bool has_extension(const std::vector<VkExtensionProperties> &list, const char *name)
{
	for (const auto &p : list)
		if (!std::strcmp(p.extensionName, name))
			return true;
	return false;
}

static void add_unique(std::vector<std::string> &list, const char *name)
{
	for (const auto &e : list)
		if (e == name)
			return;
	list.push_back(name);
}

static std::vector<const char *> pointers(const std::vector<std::string> &list)
{
	std::vector<const char *> out;
	for (const auto &s : list)
		out.push_back(s.c_str());
	return out;
}

struct EyeImage {
	VkImage image = VK_NULL_HANDLE;
	VkDeviceMemory memory = VK_NULL_HANDLE;
};

int main(int argc, char **argv)
{
	if (argc < 2) {
		std::fprintf(stderr, "usage: %s /path/to/vrclient.dylib [frames]\n", argv[0]);
		return 2;
	}
	const int frames = argc > 2 ? std::atoi(argv[2]) : 900;

	// --- xrizer, as openvr_api loads it ---------------------------------------------------
	void *library = dlopen(argv[1], RTLD_NOW | RTLD_LOCAL);
	if (!library) {
		std::fprintf(stderr, "dlopen: %s\n", dlerror());
		return 1;
	}
	using Factory = void *(*)(const char *, int *);
	auto factory = reinterpret_cast<Factory>(dlsym(library, "VRClientCoreFactory"));
	if (!factory) {
		std::fprintf(stderr, "no VRClientCoreFactory export\n");
		return 1;
	}
	int code = 0;
	auto *core = static_cast<vr::IVRClientCore *>(factory(vr::IVRClientCore_Version, &code));
	if (!core) {
		std::fprintf(stderr, "VRClientCoreFactory(%s) failed: %d\n", vr::IVRClientCore_Version, code);
		return 1;
	}
	vr::EVRInitError error = core->Init(vr::VRApplication_Scene, nullptr);
	if (error != vr::VRInitError_None) {
		std::fprintf(stderr, "Init failed: %s\n", core->GetIDForVRInitError(error));
		return 1;
	}
	auto *system = static_cast<vr::IVRSystem *>(core->GetGenericInterface(vr::IVRSystem_Version, &error));
	auto *compositor =
	    static_cast<vr::IVRCompositor *>(core->GetGenericInterface(vr::IVRCompositor_Version, &error));
	if (!system || !compositor) {
		std::fprintf(stderr, "missing %s or %s\n", vr::IVRSystem_Version, vr::IVRCompositor_Version);
		return 1;
	}
	uint32_t width = 0, height = 0;
	system->GetRecommendedRenderTargetSize(&width, &height);
	std::printf("xrizer: %s and %s, %ux%u per eye\n", vr::IVRSystem_Version, vr::IVRCompositor_Version,
	            width, height);

	// --- Vulkan through MoltenVK, with what xrizer asks for -----------------------------------
	char text[4096] = {};
	compositor->GetVulkanInstanceExtensionsRequired(text, sizeof(text));
	std::vector<std::string> instance_exts = split(text);
	std::printf("instance extensions xrizer asks for: %s\n", text);

	uint32_t count = 0;
	vkEnumerateInstanceExtensionProperties(nullptr, &count, nullptr);
	std::vector<VkExtensionProperties> available(count);
	vkEnumerateInstanceExtensionProperties(nullptr, &count, available.data());
	VkInstanceCreateFlags instance_flags = 0;
	if (has_extension(available, VK_KHR_PORTABILITY_ENUMERATION_EXTENSION_NAME)) {
		add_unique(instance_exts, VK_KHR_PORTABILITY_ENUMERATION_EXTENSION_NAME);
		instance_flags |= VK_INSTANCE_CREATE_ENUMERATE_PORTABILITY_BIT_KHR;
	}
	auto instance_names = pointers(instance_exts);
	VkApplicationInfo app = {VK_STRUCTURE_TYPE_APPLICATION_INFO};
	app.pApplicationName = "xrizer macOS probe";
	app.apiVersion = VK_API_VERSION_1_1;
	VkInstanceCreateInfo instance_info = {VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO};
	instance_info.flags = instance_flags;
	instance_info.pApplicationInfo = &app;
	instance_info.enabledExtensionCount = (uint32_t)instance_names.size();
	instance_info.ppEnabledExtensionNames = instance_names.data();
	VkInstance instance;
	CHECK_VK(vkCreateInstance(&instance_info, nullptr, &instance));

	// The device the runtime renders with, as OpenVR apps are told to choose it.
	uint64_t output = 0;
	system->GetOutputDevice(&output, vr::TextureType_Vulkan, instance);
	VkPhysicalDevice physical = (VkPhysicalDevice)(uintptr_t)output;
	if (!physical) {
		uint32_t n = 1;
		vkEnumeratePhysicalDevices(instance, &n, &physical);
		std::printf("GetOutputDevice gave none; using the first device\n");
	}
	VkPhysicalDeviceProperties props;
	vkGetPhysicalDeviceProperties(physical, &props);
	std::printf("Vulkan device: %s\n", props.deviceName);

	compositor->GetVulkanDeviceExtensionsRequired(physical, text, sizeof(text));
	std::vector<std::string> device_exts = split(text);
	std::printf("device extensions xrizer asks for: %s\n", text);
	vkEnumerateDeviceExtensionProperties(physical, nullptr, &count, nullptr);
	std::vector<VkExtensionProperties> device_available(count);
	vkEnumerateDeviceExtensionProperties(physical, nullptr, &count, device_available.data());
	if (has_extension(device_available, "VK_KHR_portability_subset"))
		add_unique(device_exts, "VK_KHR_portability_subset");
	for (const auto &e : device_exts)
		if (!has_extension(device_available, e.c_str()))
			std::printf("warning: the device lacks %s\n", e.c_str());

	vkGetPhysicalDeviceQueueFamilyProperties(physical, &count, nullptr);
	std::vector<VkQueueFamilyProperties> families(count);
	vkGetPhysicalDeviceQueueFamilyProperties(physical, &count, families.data());
	uint32_t family = 0;
	while (family < count && !(families[family].queueFlags & VK_QUEUE_GRAPHICS_BIT))
		family++;
	float priority = 1.0f;
	VkDeviceQueueCreateInfo queue_info = {VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO};
	queue_info.queueFamilyIndex = family;
	queue_info.queueCount = 1;
	queue_info.pQueuePriorities = &priority;
	auto device_names = pointers(device_exts);
	VkDeviceCreateInfo device_info = {VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO};
	device_info.queueCreateInfoCount = 1;
	device_info.pQueueCreateInfos = &queue_info;
	device_info.enabledExtensionCount = (uint32_t)device_names.size();
	device_info.ppEnabledExtensionNames = device_names.data();
	VkDevice device;
	CHECK_VK(vkCreateDevice(physical, &device_info, nullptr, &device));
	VkQueue queue;
	vkGetDeviceQueue(device, family, 0, &queue);

	// --- one image per eye, cleared each frame --------------------------------------------
	const VkFormat format = VK_FORMAT_R8G8B8A8_SRGB;
	VkPhysicalDeviceMemoryProperties memory_props;
	vkGetPhysicalDeviceMemoryProperties(physical, &memory_props);
	EyeImage eyes[2];
	for (auto &eye : eyes) {
		VkImageCreateInfo image_info = {VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO};
		image_info.imageType = VK_IMAGE_TYPE_2D;
		image_info.format = format;
		image_info.extent = {width, height, 1};
		image_info.mipLevels = 1;
		image_info.arrayLayers = 1;
		image_info.samples = VK_SAMPLE_COUNT_1_BIT;
		image_info.tiling = VK_IMAGE_TILING_OPTIMAL;
		image_info.usage = VK_IMAGE_USAGE_TRANSFER_DST_BIT | VK_IMAGE_USAGE_TRANSFER_SRC_BIT |
		                   VK_IMAGE_USAGE_SAMPLED_BIT | VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT;
		CHECK_VK(vkCreateImage(device, &image_info, nullptr, &eye.image));
		VkMemoryRequirements req;
		vkGetImageMemoryRequirements(device, eye.image, &req);
		uint32_t type = 0;
		while (type < memory_props.memoryTypeCount &&
		       !((req.memoryTypeBits & (1u << type)) &&
		         (memory_props.memoryTypes[type].propertyFlags & VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT)))
			type++;
		VkMemoryAllocateInfo alloc = {VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO};
		alloc.allocationSize = req.size;
		alloc.memoryTypeIndex = type;
		CHECK_VK(vkAllocateMemory(device, &alloc, nullptr, &eye.memory));
		CHECK_VK(vkBindImageMemory(device, eye.image, eye.memory, 0));
	}

	VkCommandPoolCreateInfo pool_info = {VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO};
	pool_info.flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT;
	pool_info.queueFamilyIndex = family;
	VkCommandPool pool;
	CHECK_VK(vkCreateCommandPool(device, &pool_info, nullptr, &pool));
	VkCommandBufferAllocateInfo cmd_info = {VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO};
	cmd_info.commandPool = pool;
	cmd_info.level = VK_COMMAND_BUFFER_LEVEL_PRIMARY;
	cmd_info.commandBufferCount = 1;
	VkCommandBuffer cmd;
	CHECK_VK(vkAllocateCommandBuffers(device, &cmd_info, &cmd));
	VkFenceCreateInfo fence_info = {VK_STRUCTURE_TYPE_FENCE_CREATE_INFO};
	VkFence fence;
	CHECK_VK(vkCreateFence(device, &fence_info, nullptr, &fence));

	VkImageLayout layout = VK_IMAGE_LAYOUT_UNDEFINED;
	vr::TrackedDevicePose_t poses[vr::k_unMaxTrackedDeviceCount];
	for (int frame = 0; frame < frames; frame++) {
		vr::EVRCompositorError wait = compositor->WaitGetPoses(poses, vr::k_unMaxTrackedDeviceCount, nullptr, 0);
		if (wait != vr::VRCompositorError_None)
			std::printf("frame %d: WaitGetPoses error %d\n", frame, (int)wait);

		// Clear both eyes, leaving them in TRANSFER_SRC_OPTIMAL as OpenVR expects at Submit.
		VkCommandBufferBeginInfo begin = {VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO};
		begin.flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT;
		CHECK_VK(vkBeginCommandBuffer(cmd, &begin));
		const float pulse = 0.5f + 0.5f * std::sin(frame * 0.05f);
		const VkClearColorValue colours[2] = {{{pulse, 0.0f, 0.0f, 1.0f}}, {{0.0f, 0.0f, pulse, 1.0f}}};
		VkImageSubresourceRange range = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1};
		for (int e = 0; e < 2; e++) {
			VkImageMemoryBarrier to_dst = {VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER};
			to_dst.oldLayout = layout;
			to_dst.newLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL;
			to_dst.srcAccessMask = VK_ACCESS_TRANSFER_READ_BIT;
			to_dst.dstAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT;
			to_dst.srcQueueFamilyIndex = to_dst.dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED;
			to_dst.image = eyes[e].image;
			to_dst.subresourceRange = range;
			vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 0,
			                     nullptr, 0, nullptr, 1, &to_dst);
			vkCmdClearColorImage(cmd, eyes[e].image, VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, &colours[e], 1,
			                     &range);
			VkImageMemoryBarrier to_src = to_dst;
			to_src.oldLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL;
			to_src.newLayout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL;
			to_src.srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT;
			to_src.dstAccessMask = VK_ACCESS_TRANSFER_READ_BIT;
			vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 0,
			                     nullptr, 0, nullptr, 1, &to_src);
		}
		layout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL;
		CHECK_VK(vkEndCommandBuffer(cmd));
		VkSubmitInfo submit = {VK_STRUCTURE_TYPE_SUBMIT_INFO};
		submit.commandBufferCount = 1;
		submit.pCommandBuffers = &cmd;
		CHECK_VK(vkQueueSubmit(queue, 1, &submit, fence));
		CHECK_VK(vkWaitForFences(device, 1, &fence, VK_TRUE, UINT64_MAX));
		CHECK_VK(vkResetFences(device, 1, &fence));

		for (int e = 0; e < 2; e++) {
			vr::VRVulkanTextureData_t data = {};
			data.m_nImage = (uint64_t)eyes[e].image;
			data.m_pDevice = device;
			data.m_pPhysicalDevice = physical;
			data.m_pInstance = instance;
			data.m_pQueue = queue;
			data.m_nQueueFamilyIndex = family;
			data.m_nWidth = width;
			data.m_nHeight = height;
			data.m_nFormat = format;
			data.m_nSampleCount = 1;
			vr::Texture_t texture = {&data, vr::TextureType_Vulkan, vr::ColorSpace_Auto};
			vr::EVRCompositorError result = compositor->Submit(e == 0 ? vr::Eye_Left : vr::Eye_Right, &texture);
			if (result != vr::VRCompositorError_None)
				std::printf("frame %d: Submit(%s) error %d\n", frame, e == 0 ? "left" : "right", (int)result);
		}
		compositor->PostPresentHandoff();

		if (frame % 90 == 0) {
			const auto &hmd = poses[vr::k_unTrackedDeviceIndex_Hmd];
			const auto &m = hmd.mDeviceToAbsoluteTracking.m;
			std::printf("frame %d: HMD %s at (%.3f, %.3f, %.3f)\n", frame, hmd.bPoseIsValid ? "valid" : "invalid",
			            m[0][3], m[1][3], m[2][3]);
		}
	}

	vkDeviceWaitIdle(device);
	core->Cleanup();
	vkDestroyFence(device, fence, nullptr);
	vkDestroyCommandPool(device, pool, nullptr);
	for (auto &eye : eyes) {
		vkDestroyImage(device, eye.image, nullptr);
		vkFreeMemory(device, eye.memory, nullptr);
	}
	vkDestroyDevice(device, nullptr);
	vkDestroyInstance(instance, nullptr);
	std::printf("done: %d frames\n", frames);
	return 0;
}
