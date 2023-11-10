#!/usr/bin/env python3

def parse_type_signature(signature):
    tree = []  # The root of the tree will be an empty list initially
    stack = [tree]  # Start with the root in the stack
    buffer = ''  # Buffer to accumulate type name characters
    indent = 0
    escape = False

    for char in signature:
        if char == '<' and not escape:  # Start of type parameters
            name = buffer.strip()
            indent += 1
            node = {'name': name, 'children': []}
            # Add the new node to the tree or sub-tree
            stack[-1].append(node)
            # Push the 'children' of the new node onto the stack to start a new sub-tree
            stack.append(node['children'])
            # Clear the buffer for the next type name
            buffer = ''
        elif char == '>' and not escape:  # End of type parameters
            if buffer.strip():
                # If there's a type name in the buffer, create a node for it without children
                stack[-1].append({'name': buffer.strip(), 'children': []})
                buffer = ''
            # Pop the last sub-tree from the stack, ending the current sub-tree
            stack.pop()
            indent -= 1
        elif char == ',' and not escape:  # Separator between type parameters
            if buffer:
                # If there's a type name in the buffer, create a node for it without children
                stack[-1].append({'name': buffer.strip(), 'children': []})
                buffer = ''
        elif char == '.':
            escape = True
            buffer += char
        else:  # Regular character
            buffer += char
            escape = False

    # If there's any buffer left after the loop, it's a type name that needs a node
    if buffer:
        stack[-1].append({'name': buffer, 'children': []})

    return tree[0] if tree else []

def set_size(node):
    size = len(node['name'])
    for child in node['children']:
        size += set_size(child)
    node['size'] = size
    return size

def humanize(bytes):
    if bytes < 1024:
        return f"{bytes} B"
    elif bytes < 1024 * 1024:
        return f"{bytes / 1024:.1f} KB"
    elif bytes < 1024 * 1024 * 1024:
        return f"{bytes / 1024 / 1024:.1f} MB"
    else:
        return f"{bytes / 1024 / 1024 / 1024:.1f} GB"

def print_tree(tree, total_size, indent=0):
    percent = tree['size'] / total_size * 100.0
    bytes = humanize(tree['size'])
    print(' ' * indent + tree['name'] + f" [{bytes} {percent:.0f}%]")
    for n in tree['children']:
        set_size(n)
    if percent < 5:
        return
    for n in tree['children']: #sorted(tree['children'], key=lambda n: n['size'], reverse=True):
        print_tree(n, total_size, indent = indent + 4)

if __name__ == '__main__':
    import sys

    for line in sys.stdin:
        print("# " + str(len(line)) + " " + line[0:120])
        sym = line.split(' ', 2)[2]
        if not sym:
            continue
        type_tree = parse_type_signature(sym)
        type_tree['size'] = set_size(type_tree)
        print_tree(type_tree, type_tree['size'])
